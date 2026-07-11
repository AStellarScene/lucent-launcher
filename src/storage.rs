//! Application-owned filesystem and profile persistence.
//!
//! This module keeps app-data migration and durable profile storage away from
//! GTK callbacks. The launcher core owns Minecraft installation primitives;
//! this module owns Lucent's application-level data contract.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::LauncherProfile;

const PROFILES_FILE_NAME: &str = "profiles.json";
const DATA_DIR_ENV: &str = "LUCENT_DATA_DIR";
const APP_DATA_SUBDIR: &str = "lucent-launcher";
const PROFILE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ProfilesDocument {
    schema_version: u32,
    profiles: Vec<LauncherProfile>,
}

/// Generates a stable-enough local profile identifier for migrated profiles.
///
/// IDs are persisted immediately by [`load_profiles_from_disk`]. The counter
/// only disambiguates profiles created during the same clock tick.
pub(crate) fn new_profile_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("profile-{timestamp:x}-{counter:x}")
}

pub(crate) fn resolve_runtime_base_dir() -> Result<PathBuf, String> {
    if let Ok(override_dir) = std::env::var(DATA_DIR_ENV) {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            fs::create_dir_all(&path)
                .map_err(|e| format!("failed creating runtime dir '{}': {e}", path.display()))?;
            return Ok(path);
        }
    }

    let legacy_base = std::env::current_dir()
        .map_err(|e| format!("failed resolving current dir for runtime fallback: {e}"))?;

    let mut preferred_base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| legacy_base.clone());
    preferred_base.push(APP_DATA_SUBDIR);

    fs::create_dir_all(&preferred_base).map_err(|e| {
        format!(
            "failed creating app data runtime dir '{}': {e}",
            preferred_base.display()
        )
    })?;

    let preferred_has_data = preferred_base.join(PROFILES_FILE_NAME).exists()
        || preferred_base.join(".minecraft").exists();
    let legacy_has_data =
        legacy_base.join(PROFILES_FILE_NAME).exists() || legacy_base.join(".minecraft").exists();

    if !preferred_has_data && legacy_has_data {
        let legacy_profiles = legacy_base.join(PROFILES_FILE_NAME);
        let preferred_profiles = preferred_base.join(PROFILES_FILE_NAME);
        if legacy_profiles.exists() && !preferred_profiles.exists() {
            fs::copy(&legacy_profiles, &preferred_profiles).map_err(|e| {
                format!(
                    "failed migrating legacy profiles '{}' -> '{}': {e}",
                    legacy_profiles.display(),
                    preferred_profiles.display()
                )
            })?;
        }

        let legacy_mc = legacy_base.join(".minecraft");
        let preferred_mc = preferred_base.join(".minecraft");
        if legacy_mc.exists()
            && !preferred_mc.exists()
            && let Err(e) = fs::rename(&legacy_mc, &preferred_mc)
        {
            eprintln!(
                "[WARN] Failed migrating legacy .minecraft dir '{}' -> '{}': {e}. Falling back to legacy runtime path.",
                legacy_mc.display(),
                preferred_mc.display()
            );
            return Ok(legacy_base);
        }
    }

    Ok(preferred_base)
}

pub(crate) fn minecraft_root_dir() -> Result<PathBuf, String> {
    let root = resolve_runtime_base_dir()?.join(".minecraft");
    fs::create_dir_all(&root)
        .map_err(|e| format!("failed creating minecraft dir '{}': {e}", root.display()))?;
    Ok(root)
}

fn profiles_file_path() -> Result<PathBuf, String> {
    Ok(resolve_runtime_base_dir()?.join(PROFILES_FILE_NAME))
}

pub(crate) fn load_profiles_from_disk() -> Result<Vec<LauncherProfile>, String> {
    let path = profiles_file_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content =
        fs::read_to_string(&path).map_err(|e| format!("failed reading {}: {e}", path.display()))?;
    let raw: Value = serde_json::from_str(&content)
        .map_err(|e| format!("failed parsing {}: {e}", path.display()))?;

    let legacy_format = raw.is_array();
    let mut profiles = if legacy_format {
        serde_json::from_value::<Vec<LauncherProfile>>(raw)
            .map_err(|e| format!("failed parsing legacy profiles {}: {e}", path.display()))?
    } else {
        let document: ProfilesDocument = serde_json::from_value(raw)
            .map_err(|e| format!("failed parsing profile document {}: {e}", path.display()))?;
        if document.schema_version > PROFILE_SCHEMA_VERSION {
            return Err(format!(
                "profile document {} uses unsupported schema version {}",
                path.display(),
                document.schema_version
            ));
        }
        document.profiles
    };

    // Rewriting on load upgrades the legacy array format and persists generated
    // IDs before profile directories are used.
    let mut generated_ids = false;
    for profile in &mut profiles {
        if profile.id.is_empty() {
            profile.id = new_profile_id();
            generated_ids = true;
        }
    }
    if legacy_format || generated_ids {
        save_profiles_to_disk(&profiles)?;
    }
    Ok(profiles)
}

pub(crate) fn save_profiles_to_disk(profiles: &[LauncherProfile]) -> Result<(), String> {
    let path = profiles_file_path()?;
    let document = ProfilesDocument {
        schema_version: PROFILE_SCHEMA_VERSION,
        profiles: profiles.to_vec(),
    };
    let content = serde_json::to_vec_pretty(&document)
        .map_err(|e| format!("failed serializing profiles: {e}"))?;
    mc_launcher_core::io::atomic::write_bytes(&path, &content)
        .map_err(|e| format!("failed writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::new_profile_id;

    #[test]
    fn generated_profile_ids_are_distinct() {
        assert_ne!(new_profile_id(), new_profile_id());
    }
}
