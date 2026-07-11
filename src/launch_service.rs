//! GTK-free launch orchestration.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::messages::{AppEvent, LaunchEvent, LaunchProgress};
use crate::{
    LauncherProfile, MS_CLIENT_ID_ENV, ProfileLoader, ProfileLoaderVersion, Session,
    apply_profile_runtime_jvm_overrides, auto_repair_profile_mods,
    complete_microsoft_refresh_resilient, discover_java_from_env,
    ensure_maven_fallback_libraries_present, ensure_runtime_java_for_version,
    install_forge_profile_with_java, minecraft_root_dir, profile_game_directory,
    resolve_latest_forge_version_for_minecraft, resolve_preferred_java_executable,
    save_refresh_token,
};

struct HeldFileLock {
    _file: File,
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("failed opening launch lock '{}': {e}", path.display()))
}

fn acquire_file_lock(path: &Path, busy_message: String) -> Result<HeldFileLock, String> {
    let file = open_lock_file(path)?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => busy_message,
        std::fs::TryLockError::Error(error) => {
            format!("failed locking '{}': {error}", path.display())
        }
    })?;

    Ok(HeldFileLock { _file: file })
}

fn acquire_profile_launch_lock(profile: &LauncherProfile) -> Result<HeldFileLock, String> {
    let game_dir = profile_game_directory(profile)?;
    let lock_path = game_dir.join(".lucent-launch.lock");
    acquire_file_lock(
        &lock_path,
        format!(
            "Profile '{}' is already running in another launcher instance.",
            profile.name
        ),
    )
}

fn acquire_shared_install_lock(minecraft_dir: &Path) -> Result<HeldFileLock, String> {
    let lock_path = minecraft_dir.join(".lucent-install.lock");
    let file = open_lock_file(&lock_path)?;
    file.lock()
        .map_err(|error| format!("failed locking shared Minecraft installation: {error}"))?;
    Ok(HeldFileLock { _file: file })
}

fn install_stage_label(stage: &mc_launcher_core::progress::InstallStage) -> &'static str {
    use mc_launcher_core::progress::InstallStage;

    match stage {
        InstallStage::ResolveVersion => "Resolving version metadata",
        InstallStage::DownloadLibraries => "Downloading libraries",
        InstallStage::DownloadAssets => "Downloading assets",
        InstallStage::InstallRuntime => "Installing Java runtime",
        InstallStage::ExtractNatives => "Extracting native libraries",
        InstallStage::LoaderInstall => "Installing loader",
        InstallStage::Verify => "Verifying installation",
    }
}

fn core_progress_reporter(
    tx: &mpsc::Sender<AppEvent>,
    profile_id: String,
) -> impl FnMut(mc_launcher_core::progress::ProgressEvent) + '_ {
    use mc_launcher_core::progress::ProgressEvent;

    let mut stage = "Preparing installation".to_string();
    let mut completed_tasks = 0_u64;
    let mut total_tasks = None;
    let mut current_task = None;
    let mut bytes_received = 0_u64;
    let mut total_bytes = None;

    move |event| {
        match event {
            ProgressEvent::StageStarted { stage: next_stage } => {
                stage = install_stage_label(&next_stage).to_string();
                completed_tasks = 0;
                total_tasks = None;
                bytes_received = 0;
                total_bytes = None;
            }
            ProgressEvent::PlanStarted { total_tasks: total } => {
                total_tasks = Some(total);
                completed_tasks = 0;
                current_task = None;
                bytes_received = 0;
                total_bytes = None;
            }
            ProgressEvent::TaskStarted { label, .. } => {
                current_task = Some(label);
                bytes_received = 0;
                total_bytes = None;
            }
            ProgressEvent::TaskSkipped { label, .. } => {
                completed_tasks = completed_tasks.saturating_add(1);
                current_task = Some(label);
                bytes_received = 0;
                total_bytes = None;
            }
            ProgressEvent::TaskFinished { label } => {
                completed_tasks = completed_tasks.saturating_add(1);
                current_task = Some(label);
                if let Some(total) = total_bytes {
                    bytes_received = total;
                }
            }
            ProgressEvent::BytesReceived {
                label,
                received,
                total,
            } => {
                current_task = Some(label);
                bytes_received = received;
                total_bytes = total;
            }
        }

        let _ = tx.send(AppEvent::Launch(LaunchEvent::Progress(LaunchProgress {
            profile_id: profile_id.clone(),
            stage: stage.clone(),
            completed_tasks,
            total_tasks,
            current_task: current_task.clone(),
            bytes_received,
            total_bytes,
        })));
    }
}

fn send_indeterminate_progress(
    tx: &mpsc::Sender<AppEvent>,
    profile_id: &str,
    stage: impl Into<String>,
) {
    let _ = tx.send(AppEvent::Launch(LaunchEvent::Progress(LaunchProgress {
        profile_id: profile_id.to_string(),
        stage: stage.into(),
        completed_tasks: 0,
        total_tasks: None,
        current_task: None,
        bytes_received: 0,
        total_bytes: None,
    })));
}

fn is_game_ready_line(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("backend library:")
        || line.contains("opengl version")
        || line.contains("sound engine started")
}

fn forward_process_line(
    tx: &mpsc::Sender<AppEvent>,
    line: String,
    prefix: Option<&str>,
    game_ready: &Arc<AtomicBool>,
    profile_id: &str,
) {
    if is_game_ready_line(&line) && !game_ready.swap(true, Ordering::AcqRel) {
        let _ = tx.send(AppEvent::launch_ready(profile_id));
    }

    let output = match prefix {
        Some(prefix) => format!("{prefix}{line}"),
        None => line,
    };
    let _ = tx.send(AppEvent::Log(output));
}

/// Owns the blocking preparation and execution pipeline for one profile.
pub(crate) struct LaunchService;

impl LaunchService {
    /// Installs/prepares the selected profile and waits for the Java process.
    ///
    /// This service intentionally has no GTK dependency. Progress and process
    /// output are sent through typed application events.
    pub(crate) fn run(
        selected_profile: LauncherProfile,
        session: Session,
        java_path_raw: String,
        java_install_policy: mc_launcher_core::install::JavaInstallPolicy,
        thread_tx: mpsc::Sender<AppEvent>,
    ) {
        use mc_launcher_core::account::Account;
        use mc_launcher_core::prelude::*;

        let _profile_lock = match acquire_profile_launch_lock(&selected_profile) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = thread_tx.send(AppEvent::launch_failed(selected_profile.id.clone(), error));
                return;
            }
        };

        let mut effective_session = session;
        if let Session::Microsoft {
            refresh_token,
            username,
            ..
        } = &effective_session
        {
            if let Ok(client_id) = std::env::var(MS_CLIENT_ID_ENV) {
                let _ = thread_tx.send(AppEvent::Log(format!(
                    "Refreshing Microsoft access token for {username}..."
                )));
                match complete_microsoft_refresh_resilient(&client_id, refresh_token) {
                    Ok((fresh_username, fresh_uuid, fresh_access_token, fresh_refresh_token)) => {
                        if let Err(e) = save_refresh_token(&fresh_refresh_token) {
                            let _ = thread_tx.send(AppEvent::Log(format!(
                                "[WARN] Failed updating stored refresh token: {e}"
                            )));
                        }
                        effective_session = Session::Microsoft {
                            username: fresh_username,
                            uuid: fresh_uuid,
                            access_token: fresh_access_token,
                            refresh_token: fresh_refresh_token,
                        };
                    }
                    Err(e) => {
                        let _ = thread_tx.send(AppEvent::launch_failed(
                            selected_profile.id.clone(),
                            format!("Microsoft token refresh failed before launch: {e}"),
                        ));
                        return;
                    }
                }
            } else {
                let _ = thread_tx.send(AppEvent::Log(
                    "[WARN] LUCENT_MS_CLIENT_ID missing; using existing Microsoft token"
                        .to_string(),
                ));
            }
        }

        thread_tx
            .send(AppEvent::StatusUpdate(format!(
                "Preparing {} ({})",
                selected_profile.name, selected_profile.version_id
            )))
            .unwrap();
        thread_tx
            .send(AppEvent::Log(format!(
                "Initializing launch pipeline for user: {} (profile: {})",
                effective_session.display_name(),
                selected_profile.name
            )))
            .unwrap();

        let preferred_java = match resolve_preferred_java_executable(&java_path_raw) {
            Ok(java) => java,
            Err(e) => {
                let _ = thread_tx.send(AppEvent::launch_failed(
                    selected_profile.id.clone(),
                    format!("Java configuration error: {}", e),
                ));
                return;
            }
        };
        if let Some(java) = &preferred_java {
            let _ = thread_tx.send(AppEvent::Log(format!(
                "Using Java executable: {}",
                java.display()
            )));
        } else {
            let _ = thread_tx.send(AppEvent::Log(
                "No explicit Java binary configured; relying on launcher/runtime defaults"
                    .to_string(),
            ));
        }

        let mc_dir = match minecraft_root_dir() {
            Ok(path) => path,
            Err(e) => {
                let _ = thread_tx.send(AppEvent::launch_failed(
                    selected_profile.id.clone(),
                    format!("Failed resolving runtime minecraft directory: {e}"),
                ));
                return;
            }
        };
        let launcher = Launcher::new(mc_dir.clone());

        thread_tx
            .send(AppEvent::Log(format!(
                "Checking manifests and installing profile: MC={}, Loader={}, LoaderVersion={}",
                selected_profile.version_id,
                selected_profile.loader_label(),
                selected_profile.loader_version_label()
            )))
            .unwrap();

        let install_lock = match acquire_shared_install_lock(&mc_dir) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = thread_tx.send(AppEvent::launch_failed(selected_profile.id.clone(), error));
                return;
            }
        };

        let resolved_forge_version =
            match (&selected_profile.loader, &selected_profile.loader_version) {
                (ProfileLoader::Forge, ProfileLoaderVersion::LatestStable)
                | (ProfileLoader::Forge, ProfileLoaderVersion::Latest) => {
                    match resolve_latest_forge_version_for_minecraft(&selected_profile.version_id) {
                        Ok(forge_version) => {
                            let _ = thread_tx.send(AppEvent::Log(format!(
                                "Resolved Forge version for {} -> {}",
                                selected_profile.version_id, forge_version
                            )));
                            Some(forge_version)
                        }
                        Err(e) => {
                            let _ = thread_tx
                                .send(AppEvent::launch_failed(selected_profile.id.clone(), e));
                            return;
                        }
                    }
                }
                (ProfileLoader::Forge, ProfileLoaderVersion::Exact(v)) => Some(v.clone()),
                _ => None,
            };

        let installed_version_id = if let Some(forge_version) = resolved_forge_version {
            let _ = thread_tx.send(AppEvent::Log(
                "Starting Forge installation pipeline (this can take a few minutes)...".to_string(),
            ));
            send_indeterminate_progress(
                &thread_tx,
                &selected_profile.id,
                "Running Forge installer",
            );
            let forge_java_path = preferred_java
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| java_path_raw.clone());
            let install_result = install_forge_profile_with_java(
                &launcher,
                &selected_profile.version_id,
                &forge_version,
                &forge_java_path,
                selected_profile.java_auto_download,
                &thread_tx,
            );

            match install_result {
                Ok(version_id) => version_id,
                Err(e) => {
                    let _ = thread_tx.send(AppEvent::launch_failed(
                        selected_profile.id.clone(),
                        format!("Installation pipeline aborted: {}", e),
                    ));
                    return;
                }
            }
        } else {
            let loader_spec = match (&selected_profile.loader, &selected_profile.loader_version) {
                (ProfileLoader::Vanilla, _) => None,
                (ProfileLoader::Fabric, ProfileLoaderVersion::LatestStable) => {
                    Some(LoaderSpec::Fabric {
                        version: LoaderVersion::LatestStable,
                    })
                }
                (ProfileLoader::Fabric, ProfileLoaderVersion::Latest) => Some(LoaderSpec::Fabric {
                    version: LoaderVersion::Latest,
                }),
                (ProfileLoader::Fabric, ProfileLoaderVersion::Exact(v)) => {
                    Some(LoaderSpec::Fabric {
                        version: LoaderVersion::Exact(v.clone()),
                    })
                }
                (ProfileLoader::Quilt, ProfileLoaderVersion::LatestStable) => {
                    Some(LoaderSpec::Quilt {
                        version: LoaderVersion::LatestStable,
                    })
                }
                (ProfileLoader::Quilt, ProfileLoaderVersion::Latest) => Some(LoaderSpec::Quilt {
                    version: LoaderVersion::Latest,
                }),
                (ProfileLoader::Quilt, ProfileLoaderVersion::Exact(v)) => Some(LoaderSpec::Quilt {
                    version: LoaderVersion::Exact(v.clone()),
                }),
                (ProfileLoader::NeoForge, ProfileLoaderVersion::LatestStable) => {
                    Some(LoaderSpec::NeoForge {
                        version: LoaderVersion::LatestStable,
                    })
                }
                (ProfileLoader::NeoForge, ProfileLoaderVersion::Latest) => {
                    Some(LoaderSpec::NeoForge {
                        version: LoaderVersion::Latest,
                    })
                }
                (ProfileLoader::NeoForge, ProfileLoaderVersion::Exact(v)) => {
                    Some(LoaderSpec::NeoForge {
                        version: LoaderVersion::Exact(v.clone()),
                    })
                }
                (ProfileLoader::Forge, _) => unreachable!("Forge handled in custom installer path"),
            };

            let install_req = InstallRequest {
                minecraft_version: selected_profile.version_id.clone(),
                loader: loader_spec,
                java: java_install_policy,
            };

            let _ = thread_tx.send(AppEvent::Log(
                "Installing Minecraft/loader assets (this can take a few minutes on first run)..."
                    .to_string(),
            ));
            let mut progress_reporter =
                core_progress_reporter(&thread_tx, selected_profile.id.clone());
            let install_result =
                launcher.install_with_progress(install_req, &mut progress_reporter);

            match install_result {
                Ok(install_res) => install_res.version_id,
                Err(e) => {
                    let _ = thread_tx.send(AppEvent::launch_failed(
                        selected_profile.id.clone(),
                        format!("Installation pipeline aborted: {:?}", e),
                    ));
                    return;
                }
            }
        };

        thread_tx
            .send(AppEvent::Log(format!(
                "Successfully prepared profile: {}",
                installed_version_id,
            )))
            .unwrap();

        send_indeterminate_progress(
            &thread_tx,
            &selected_profile.id,
            "Loading installed version metadata",
        );
        let load_result = launcher.load_version(&installed_version_id);

        match load_result {
            Ok(version_meta) => {
                if let Err(e) = ensure_maven_fallback_libraries_present(
                    &version_meta,
                    launcher.minecraft_dir(),
                    &thread_tx,
                ) {
                    let _ = thread_tx.send(AppEvent::launch_failed(
                        selected_profile.id.clone(),
                        format!("Failed preparing fallback libraries: {e}"),
                    ));
                    return;
                }

                let launch_account = match effective_session {
                    Session::Offline { username } => Account::offline(username),
                    Session::Microsoft {
                        username,
                        uuid,
                        access_token,
                        refresh_token: _,
                    } => Account::Microsoft {
                        username,
                        uuid,
                        access_token,
                    },
                };

                let mut options = LaunchOptions {
                    account: launch_account,
                    ..Default::default()
                };

                match profile_game_directory(&selected_profile) {
                    Ok(game_dir) => {
                        let _ =
                            thread_tx.send(AppEvent::StatusUpdate("Repairing mods".to_string()));

                        match auto_repair_profile_mods(&selected_profile, &thread_tx) {
                            Ok(summary) => {
                                let _ = thread_tx.send(AppEvent::Log(format!(
                                                "Auto-repair summary: checked={}, updated={}, disabled={}, unknown={}",
                                                summary.checked, summary.updated, summary.disabled, summary.unknown
                                            )));

                                if !summary.disabled_mods.is_empty() {
                                    let _ = thread_tx.send(AppEvent::launch_failed(
                                                    selected_profile.id.clone(),
                                                    format!("Mod repair disabled incompatible mods with no compatible replacements for profile '{}': {}",
                                                    selected_profile.name,
                                                    summary.disabled_mods.join(", ")),
                                                ));
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = thread_tx.send(AppEvent::launch_failed(
                                    selected_profile.id.clone(),
                                    format!("Failed during mod compatibility auto-repair: {e}"),
                                ));
                                return;
                            }
                        }

                        let mods_dir = game_dir.join("mods");
                        let shaders_dir = game_dir.join("shaderpacks");

                        let count_enabled = |dir: &Path| -> usize {
                            fs::read_dir(dir)
                                .ok()
                                .into_iter()
                                .flatten()
                                .filter_map(|entry| entry.ok())
                                .map(|e| e.path())
                                .filter(|p| p.is_file())
                                .filter(|p| {
                                    p.file_name()
                                        .and_then(|n| n.to_str())
                                        .map(|n| !n.ends_with(".disabled"))
                                        .unwrap_or(false)
                                })
                                .count()
                        };

                        let _ = thread_tx.send(AppEvent::Log(format!(
                            "Using profile game directory: {}",
                            game_dir.display()
                        )));
                        let _ = thread_tx.send(AppEvent::Log(format!(
                            "Profile content: {} enabled mod(s), {} enabled shaderpack(s)",
                            count_enabled(&mods_dir),
                            count_enabled(&shaders_dir)
                        )));

                        options.game_directory = Some(game_dir);
                    }
                    Err(e) => {
                        let _ = thread_tx.send(AppEvent::launch_failed(
                            selected_profile.id.clone(),
                            format!("Failed preparing profile game directory: {e}"),
                        ));
                        return;
                    }
                }

                let mut launch_java = preferred_java.clone();
                if launch_java.is_none() && selected_profile.java_auto_download {
                    match ensure_runtime_java_for_version(
                        launcher.minecraft_dir(),
                        &installed_version_id,
                        Some(&version_meta),
                        &thread_tx,
                    ) {
                        Ok(Some(path)) => launch_java = Some(path),
                        Ok(None) => {
                            let _ = thread_tx.send(AppEvent::Log(
                                            "No version-specific managed Java runtime metadata found; falling back to system discovery".to_string(),
                                        ));
                        }
                        Err(e) => {
                            let _ = thread_tx.send(AppEvent::launch_failed(
                                selected_profile.id.clone(),
                                format!("Java runtime auto-install failed: {}", e),
                            ));
                            return;
                        }
                    }
                }

                if launch_java.is_none() {
                    launch_java = discover_java_from_env();
                }

                if let Some(java) = &launch_java {
                    options.java_executable = Some(java.clone());
                }

                match launcher.build_launch_command_from_version(&version_meta, options) {
                    Ok(mut launch_cmd) => {
                        let injected = apply_profile_runtime_jvm_overrides(
                            &mut launch_cmd,
                            version_meta.main_class.as_deref(),
                            selected_profile.java_memory_mb,
                            selected_profile.java_args.as_deref(),
                        );
                        if injected > 0 {
                            let _ = thread_tx.send(AppEvent::Log(format!(
                                "Applied {injected} profile JVM override argument(s)"
                            )));
                        }

                        let removed = launch_cmd.deduplicate_classpath();
                        if removed > 0 {
                            let _ = thread_tx.send(AppEvent::Log(format!(
                                "Deduplicated {removed} conflicting library entries from classpath"
                            )));
                        }

                        thread_tx
                            .send(AppEvent::StatusUpdate("Launching Game Engine".into()))
                            .unwrap();
                        thread_tx
                            .send(AppEvent::Log(
                                "Spawning Java runtime context process...".into(),
                            ))
                            .unwrap();

                        let _ = thread_tx.send(AppEvent::Log(format!(
                            "Launch command executable: {}",
                            launch_cmd.executable.display()
                        )));

                        let spawn_with = |exe: &Path| {
                            std::process::Command::new(exe)
                                .args(&launch_cmd.args)
                                .current_dir(&launch_cmd.working_dir)
                                .stdout(Stdio::piped())
                                .stderr(Stdio::piped())
                                .spawn()
                        };

                        // Shared installation and runtime preparation are complete. Release
                        // the global lock before allowing another profile to prepare.
                        drop(install_lock);
                        let mut child_res = spawn_with(&launch_cmd.executable);
                        if let Err(err) = &child_res
                            && err.kind() == io::ErrorKind::NotFound
                            && let Some(java) = &launch_java
                            && java != &launch_cmd.executable
                        {
                            let _ = thread_tx.send(AppEvent::Log(format!(
                                "Launch executable not found; retrying with resolved Java: {}",
                                java.display()
                            )));
                            child_res = spawn_with(java);
                        }

                        match child_res {
                            Ok(mut child) => {
                                let _ = thread_tx.send(AppEvent::StatusUpdate(
                                    "Waiting for Minecraft to initialize…".into(),
                                ));
                                let game_ready = Arc::new(AtomicBool::new(false));
                                let profile_id = selected_profile.id.clone();

                                let stdout_tx = thread_tx.clone();
                                let stdout_ready = Arc::clone(&game_ready);
                                let stdout_profile_id = profile_id.clone();
                                let stdout_reader = child.stdout.take().map(|stdout| {
                                    thread::spawn(move || {
                                        let reader = BufReader::new(stdout);
                                        for line in reader.lines() {
                                            match line {
                                                Ok(line) => forward_process_line(
                                                    &stdout_tx,
                                                    line,
                                                    None,
                                                    &stdout_ready,
                                                    &stdout_profile_id,
                                                ),
                                                Err(e) => {
                                                    let _ = stdout_tx.send(AppEvent::Log(format!(
                                                        "[stdout read error] {e}"
                                                    )));
                                                    break;
                                                }
                                            }
                                        }
                                    })
                                });

                                let stderr_tx = thread_tx.clone();
                                let stderr_ready = Arc::clone(&game_ready);
                                let stderr_profile_id = profile_id.clone();
                                let stderr_reader = child.stderr.take().map(|stderr| {
                                    thread::spawn(move || {
                                        let reader = BufReader::new(stderr);
                                        for line in reader.lines() {
                                            match line {
                                                Ok(line) => forward_process_line(
                                                    &stderr_tx,
                                                    line,
                                                    Some("[stderr] "),
                                                    &stderr_ready,
                                                    &stderr_profile_id,
                                                ),
                                                Err(e) => {
                                                    let _ = stderr_tx.send(AppEvent::Log(format!(
                                                        "[stderr read error] {e}"
                                                    )));
                                                    break;
                                                }
                                            }
                                        }
                                    })
                                });

                                match child.wait() {
                                    Ok(status) => {
                                        if let Some(handle) = stdout_reader {
                                            let _ = handle.join();
                                        }
                                        if let Some(handle) = stderr_reader {
                                            let _ = handle.join();
                                        }

                                        if status.success() {
                                            let _ = thread_tx.send(AppEvent::launch_finished(
                                                profile_id.clone(),
                                            ));
                                        } else {
                                            let _ = thread_tx.send(AppEvent::launch_failed(
                                                profile_id.clone(),
                                                format!(
                                                    "Java process exited with status: {status}"
                                                ),
                                            ));
                                        }
                                    }
                                    Err(e) => {
                                        let _ = thread_tx.send(AppEvent::launch_failed(
                                            profile_id.clone(),
                                            format!("Failed waiting for Java process: {e}"),
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = thread_tx.send(AppEvent::launch_failed(
                                                selected_profile.id.clone(),
                                                format!("Failed to spawn Java execution process: {}. Configure a valid Java binary in Profile Editor > Runtime Settings, or keep Runtime Java policy on Auto to download a managed runtime.", e),
                                            ));
                            }
                        }
                    }
                    Err(e) => thread_tx
                        .send(AppEvent::launch_failed(
                            selected_profile.id.clone(),
                            format!("Launch command compilation failed: {e:?}"),
                        ))
                        .unwrap(),
                }
            }
            Err(e) => thread_tx
                .send(AppEvent::launch_failed(
                    selected_profile.id.clone(),
                    format!("Failed loading version structural profile: {e:?}"),
                ))
                .unwrap(),
        }
    }
}
