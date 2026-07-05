use adw::prelude::*;
use adw::{ActionRow, Application, ApplicationWindow, BottomSheet, EntryRow};
use gtk::{
    Box as GtkBox, Button, CheckButton, CssProvider, DropDown, Entry as GtkEntry, FlowBox, Frame,
    GestureClick, Label, Orientation, ProgressBar, Stack, StringList, Switch, TextView,
};
use keyring::{Entry, Error as KeyringError};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// Bring Libadwaita's underlying re-exported glib engine into scope
use adw::glib;

const MS_CLIENT_ID_ENV: &str = "LUCENT_MS_CLIENT_ID";
const MS_REDIRECT_URI_ENV: &str = "LUCENT_MS_REDIRECT_URI";
const DEFAULT_MS_REDIRECT_URI: &str = "http://localhost:53682/callback";
const KEYRING_SERVICE: &str = "com.lucentlauncher";
const KEYRING_ACCOUNT: &str = "microsoft-refresh-token";
const PROFILES_FILE_NAME: &str = "profiles.json";
const UI_RESOURCE_PATH: &str = "/com/lucentlauncher/ui/launcher.ui";
const DATA_DIR_ENV: &str = "LUCENT_DATA_DIR";
const APP_DATA_SUBDIR: &str = "lucent-launcher";

#[derive(Clone)]
enum Session {
    Offline {
        username: String,
    },
    Microsoft {
        username: String,
        uuid: String,
        access_token: String,
        refresh_token: String,
    },
}

impl Session {
    fn display_name(&self) -> &str {
        match self {
            Self::Offline { username } | Self::Microsoft { username, .. } => username,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ProfileLoader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ProfileLoaderVersion {
    LatestStable,
    Latest,
    Exact(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ProfileColorMode {
    Auto,
    Custom,
}

impl Default for ProfileColorMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LauncherProfile {
    name: String,
    version_id: String,
    loader: ProfileLoader,
    loader_version: ProfileLoaderVersion,
    #[serde(default)]
    color_mode: ProfileColorMode,
    #[serde(default)]
    color_hex: Option<String>,
    #[serde(default)]
    java_binary: Option<String>,
    #[serde(default = "default_java_auto_download")]
    java_auto_download: bool,
    #[serde(default)]
    java_memory_mb: Option<u32>,
    #[serde(default)]
    java_args: Option<String>,
}

fn default_java_auto_download() -> bool {
    true
}

impl LauncherProfile {
    fn default_with_version(version_id: String) -> Self {
        Self {
            name: "Default".to_string(),
            version_id,
            loader: ProfileLoader::Vanilla,
            loader_version: ProfileLoaderVersion::LatestStable,
            color_mode: ProfileColorMode::Auto,
            color_hex: None,
            java_binary: None,
            java_auto_download: true,
            java_memory_mb: None,
            java_args: None,
        }
    }

    fn loader_label(&self) -> &'static str {
        match self.loader {
            ProfileLoader::Vanilla => "Vanilla",
            ProfileLoader::Fabric => "Fabric",
            ProfileLoader::Quilt => "Quilt",
            ProfileLoader::Forge => "Forge",
            ProfileLoader::NeoForge => "NeoForge",
        }
    }

    fn loader_version_label(&self) -> String {
        match &self.loader_version {
            ProfileLoaderVersion::LatestStable => "LatestStable".to_string(),
            ProfileLoaderVersion::Latest => "Latest".to_string(),
            ProfileLoaderVersion::Exact(v) => format!("Exact({v})"),
        }
    }
}

fn parse_optional_u32(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        trimmed.parse::<u32>().ok()
    }
}

fn normalize_optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_extra_jvm_arguments(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split_whitespace()
        .map(ToString::to_string)
        .collect()
}

fn apply_profile_runtime_jvm_overrides(
    args: &mut Vec<String>,
    main_class: Option<&str>,
    memory_mb: Option<u32>,
    extra_args: Option<&str>,
) -> usize {
    let mut insert_args: Vec<String> = Vec::new();

    if let Some(mb) = memory_mb.filter(|v| *v > 0) {
        insert_args.push(format!("-Xmx{mb}M"));
    }
    insert_args.extend(parse_extra_jvm_arguments(extra_args));

    if insert_args.is_empty() {
        return 0;
    }

    let mut main_idx = main_class
        .and_then(|main| args.iter().position(|a| a == main))
        .unwrap_or(args.len());

    if memory_mb.is_some() {
        let before_main = &args[..main_idx];
        let mut retained = Vec::with_capacity(before_main.len());
        for arg in before_main {
            if !arg.starts_with("-Xmx") {
                retained.push(arg.clone());
            }
        }
        let removed = before_main.len().saturating_sub(retained.len());
        if removed > 0 {
            let mut rebuilt = retained;
            rebuilt.extend_from_slice(&args[main_idx..]);
            *args = rebuilt;
            main_idx = main_idx.saturating_sub(removed);
        }
    }

    args.splice(main_idx..main_idx, insert_args.clone());
    insert_args.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryKind {
    Mods,
    Shaders,
}

impl DiscoveryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Mods => "Mods",
            Self::Shaders => "Shaders",
        }
    }

    fn project_type_facet(self) -> &'static str {
        match self {
            Self::Mods => "mod",
            Self::Shaders => "shader",
        }
    }

    fn install_subdir(self) -> &'static str {
        match self {
            Self::Mods => "mods",
            Self::Shaders => "shaderpacks",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModrinthSearchResponse {
    hits: Vec<ModrinthSearchHitRaw>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModrinthSearchHitRaw {
    project_id: String,
    title: String,
    description: Option<String>,
}

#[derive(Debug, Clone)]
struct DiscoveryCardData {
    project_id: String,
    title: String,
    description: String,
}

#[derive(Debug, Clone)]
struct InstalledContentEntry {
    file_name: String,
    display_name: String,
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ModrinthVersion {
    #[serde(default)]
    project_id: Option<String>,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    files: Vec<ModrinthVersionFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModrinthVersionFile {
    url: String,
    filename: String,
    primary: Option<bool>,
}

#[derive(Debug, Clone)]
struct ModRepairSummary {
    checked: usize,
    updated: usize,
    disabled: usize,
    unknown: usize,
    disabled_mods: Vec<String>,
}

// Messages sent from worker threads back to UI thread.
enum LauncherMessage {
    VersionsLoaded(Vec<String>),
    Log(String),
    StatusUpdate(String),
    TaskFinished,
    TaskFailed(String),
    OpenUrl(String),
    MicrosoftAuthSuccess {
        username: String,
        uuid: String,
        access_token: String,
        refresh_token: String,
    },
    MicrosoftAuthFailed(String),
    DiscoverySearchResults {
        kind: DiscoveryKind,
        query: String,
        results: Vec<DiscoveryCardData>,
    },
    DiscoverySearchFailed {
        kind: DiscoveryKind,
        error: String,
    },
    DiscoveryInstallFinished {
        kind: DiscoveryKind,
        title: String,
        target_path: String,
    },
    DiscoveryInstalledChanged(DiscoveryKind),
}

fn main() {
    if let Err(e) = gtk::gio::resources_register_include!("lucent-launcher.gresource") {
        panic!("failed registering embedded UI resources: {e}");
    }

    let app = Application::builder()
        .application_id("com.lucentlauncher")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn keyring_entry() -> Result<Entry, String> {
    Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(|e| format!("keyring init failed: {e}"))
}

fn load_saved_refresh_token() -> Result<Option<String>, String> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(token) if token.trim().is_empty() => Ok(None),
        Ok(token) => Ok(Some(token)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring read failed: {e}")),
    }
}

fn save_refresh_token(token: &str) -> Result<(), String> {
    let entry = keyring_entry()?;
    entry
        .set_password(token)
        .map_err(|e| format!("keyring write failed: {e}"))
}

fn clear_refresh_token() -> Result<(), String> {
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(format!("keyring delete failed: {e}")),
    }
}

fn resolve_runtime_base_dir() -> Result<PathBuf, String> {
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
    let legacy_has_data = legacy_base.join(PROFILES_FILE_NAME).exists()
        || legacy_base.join(".minecraft").exists();

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
        if legacy_mc.exists() && !preferred_mc.exists() {
            if let Err(e) = fs::rename(&legacy_mc, &preferred_mc) {
                eprintln!(
                    "[WARN] Failed migrating legacy .minecraft dir '{}' -> '{}': {e}. Falling back to legacy runtime path.",
                    legacy_mc.display(),
                    preferred_mc.display()
                );
                return Ok(legacy_base);
            }
        }
    }

    Ok(preferred_base)
}

fn runtime_base_dir() -> Result<PathBuf, String> {
    resolve_runtime_base_dir()
}

fn minecraft_root_dir() -> Result<PathBuf, String> {
    let root = runtime_base_dir()?.join(".minecraft");
    fs::create_dir_all(&root)
        .map_err(|e| format!("failed creating minecraft dir '{}': {e}", root.display()))?;
    Ok(root)
}

fn profiles_file_path() -> Result<PathBuf, String> {
    Ok(runtime_base_dir()?.join(PROFILES_FILE_NAME))
}

fn load_profiles_from_disk() -> Result<Vec<LauncherProfile>, String> {
    let path = profiles_file_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content =
        fs::read_to_string(&path).map_err(|e| format!("failed reading {}: {e}", path.display()))?;
    serde_json::from_str::<Vec<LauncherProfile>>(&content)
        .map_err(|e| format!("failed parsing {}: {e}", path.display()))
}

fn save_profiles_to_disk(profiles: &[LauncherProfile]) -> Result<(), String> {
    let path = profiles_file_path()?;
    let content = serde_json::to_string_pretty(profiles)
        .map_err(|e| format!("failed serializing profiles: {e}"))?;
    fs::write(&path, content).map_err(|e| format!("failed writing {}: {e}", path.display()))
}

fn loader_from_index(index: u32) -> ProfileLoader {
    match index {
        1 => ProfileLoader::Fabric,
        2 => ProfileLoader::Quilt,
        3 => ProfileLoader::Forge,
        4 => ProfileLoader::NeoForge,
        _ => ProfileLoader::Vanilla,
    }
}

fn index_from_loader(loader: &ProfileLoader) -> u32 {
    match loader {
        ProfileLoader::Vanilla => 0,
        ProfileLoader::Fabric => 1,
        ProfileLoader::Quilt => 2,
        ProfileLoader::Forge => 3,
        ProfileLoader::NeoForge => 4,
    }
}

fn loader_version_mode_index(mode: &ProfileLoaderVersion) -> u32 {
    match mode {
        ProfileLoaderVersion::LatestStable => 0,
        ProfileLoaderVersion::Latest => 1,
        ProfileLoaderVersion::Exact(_) => 2,
    }
}

fn loader_version_from_mode_and_text(mode_idx: u32, text: &str) -> Option<ProfileLoaderVersion> {
    match mode_idx {
        0 => Some(ProfileLoaderVersion::LatestStable),
        1 => Some(ProfileLoaderVersion::Latest),
        2 => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(ProfileLoaderVersion::Exact(trimmed.to_string()))
            }
        }
        _ => Some(ProfileLoaderVersion::LatestStable),
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());

    let (r1, g1, b1) = if (0.0..1.0).contains(&hp) {
        (c, x, 0.0)
    } else if (1.0..2.0).contains(&hp) {
        (x, c, 0.0)
    } else if (2.0..3.0).contains(&hp) {
        (0.0, c, x)
    } else if (3.0..4.0).contains(&hp) {
        (0.0, x, c)
    } else if (4.0..5.0).contains(&hp) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    let m = l - c / 2.0;
    let r = ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let g = ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let b = ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (r, g, b)
}

fn normalize_hex_color(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", hex.to_uppercase()))
}

fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let normalized = normalize_hex_color(hex)?;
    let raw = normalized.trim_start_matches('#');
    let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
    Some((r, g, b))
}

fn mix_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32 * (1.0 - t) + y as f32 * t).round()).clamp(0.0, 255.0) as u8
    };
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn profile_gradient_colors(
    profile: &LauncherProfile,
) -> ((u8, u8, u8), (u8, u8, u8), (u8, u8, u8)) {
    if profile.color_mode == ProfileColorMode::Custom {
        if let Some(hex) = profile.color_hex.as_deref() {
            if let Some(base) = hex_to_rgb(hex) {
                let c1 = mix_rgb(base, (255, 255, 255), 0.28);
                let c2 = base;
                let c3 = mix_rgb(base, (16, 20, 28), 0.30);
                return (c1, c2, c3);
            }
        }
    }

    let mut hash: u32 = 0;
    for b in profile.name.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*b as u32);
    }

    let hue = (hash % 360) as f32;
    let hue2 = (hue + 28.0) % 360.0;
    let hue3 = (hue + 54.0) % 360.0;

    let c1 = hsl_to_rgb(hue, 0.52, 0.72);
    let c2 = hsl_to_rgb(hue2, 0.48, 0.64);
    let c3 = hsl_to_rgb(hue3, 0.42, 0.58);
    (c1, c2, c3)
}

#[allow(deprecated)]
fn apply_glass_card_gradient(card: &Frame, profile: &LauncherProfile) {
    let ((r1, g1, b1), (r2, g2, b2), (r3, g3, b3)) = profile_gradient_colors(profile);
    let css = format!(
        ".profile-glass-card {{
            border-radius: 12px;
            border: 1px solid alpha(@theme_fg_color, 0.15);
            background-image:
                linear-gradient(160deg,
                    rgba({r1}, {g1}, {b1}, 0.44) 0%,
                    rgba({r2}, {g2}, {b2}, 0.30) 52%,
                    rgba({r3}, {g3}, {b3}, 0.22) 100%
                ),
                linear-gradient(180deg,
                    rgba(255, 255, 255, 0.18) 0%,
                    rgba(255, 255, 255, 0.00) 42%
                );
            box-shadow: 0 8px 22px rgba(0, 0, 0, 0.12);
            transition: box-shadow 160ms ease, border-color 160ms ease, filter 160ms ease;
        }}
        .profile-glass-card:hover {{
            border-color: alpha(@theme_fg_color, 0.24);
            box-shadow: 0 12px 28px rgba(0, 0, 0, 0.18);
            filter: brightness(1.04);
        }}
        .profile-glass-card > border {{
            border-radius: 12px;
        }}"
    );

    let provider = CssProvider::new();
    provider.load_from_data(&css);
    card.style_context()
        .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    card.add_css_class("profile-glass-card");
}

#[allow(deprecated)]
fn ensure_discovery_card_css(frame: &Frame) {
    let css = ".discovery-card {
            border-radius: 12px;
            border: 1px solid alpha(@theme_fg_color, 0.18);
            background: alpha(@theme_bg_color, 0.35);
            box-shadow: 0 8px 24px rgba(0, 0, 0, 0.14);
            transition: box-shadow 140ms ease, filter 140ms ease;
        }
        .discovery-card:hover {
            box-shadow: 0 12px 28px rgba(0, 0, 0, 0.2);
            filter: brightness(1.02);
        }
        .discovery-card > border {
            border-radius: 12px;
        }
        .discovery-select-check check {
            min-width: 18px;
            min-height: 18px;
            border-radius: 3px;
        }";

    let provider = CssProvider::new();
    provider.load_from_data(css);
    frame
        .style_context()
        .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    frame.add_css_class("discovery-card");
}

fn sanitize_component_for_path(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn profile_game_directory(profile: &LauncherProfile) -> Result<PathBuf, String> {
    let root = minecraft_root_dir()?
        .join("profiles")
        .join(sanitize_component_for_path(&profile.name));
    fs::create_dir_all(&root).map_err(|e| format!("failed creating '{}': {e}", root.display()))?;
    Ok(root)
}

fn profile_content_dir(profile: &LauncherProfile, kind: DiscoveryKind) -> Result<PathBuf, String> {
    let root = profile_game_directory(profile)?;
    let dir = root.join(kind.install_subdir());
    fs::create_dir_all(&dir).map_err(|e| format!("failed creating '{}': {e}", dir.display()))?;
    Ok(dir)
}

fn strip_disabled_suffix(name: &str) -> &str {
    name.strip_suffix(".disabled").unwrap_or(name)
}

fn list_profile_content_entries(
    profile: &LauncherProfile,
    kind: DiscoveryKind,
) -> Result<Vec<InstalledContentEntry>, String> {
    let dir = profile_content_dir(profile, kind)?;
    let mut entries = Vec::new();

    for entry in
        fs::read_dir(&dir).map_err(|e| format!("failed reading '{}': {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| format!("failed iterating '{}': {e}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let enabled = !name.ends_with(".disabled");
        let display_name = strip_disabled_suffix(name).to_string();
        entries.push(InstalledContentEntry {
            file_name: display_name.clone(),
            display_name,
            enabled,
        });
    }

    entries.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    Ok(entries)
}

fn set_profile_content_enabled(
    profile: &LauncherProfile,
    kind: DiscoveryKind,
    file_name: &str,
    enabled: bool,
) -> Result<(), String> {
    let dir = profile_content_dir(profile, kind)?;
    let enabled_path = dir.join(file_name);
    let disabled_path = dir.join(format!("{file_name}.disabled"));

    if enabled {
        if disabled_path.exists() {
            fs::rename(&disabled_path, &enabled_path).map_err(|e| {
                format!(
                    "failed enabling '{}' for profile '{}': {e}",
                    file_name, profile.name
                )
            })?;
        }
    } else if enabled_path.exists() {
        fs::rename(&enabled_path, &disabled_path).map_err(|e| {
            format!(
                "failed disabling '{}' for profile '{}': {e}",
                file_name, profile.name
            )
        })?;
    }

    Ok(())
}

fn delete_profile_content(
    profile: &LauncherProfile,
    kind: DiscoveryKind,
    file_name: &str,
) -> Result<(), String> {
    let dir = profile_content_dir(profile, kind)?;
    let enabled_path = dir.join(file_name);
    let disabled_path = dir.join(format!("{file_name}.disabled"));

    if enabled_path.exists() {
        fs::remove_file(&enabled_path).map_err(|e| {
            format!(
                "failed deleting '{}' for profile '{}': {e}",
                file_name, profile.name
            )
        })?;
    }
    if disabled_path.exists() {
        fs::remove_file(&disabled_path).map_err(|e| {
            format!(
                "failed deleting '{}' for profile '{}': {e}",
                file_name, profile.name
            )
        })?;
    }

    Ok(())
}

fn fetch_modrinth_projects(
    kind: DiscoveryKind,
    query: &str,
    profile: Option<&LauncherProfile>,
) -> Result<Vec<DiscoveryCardData>, String> {
    if kind == DiscoveryKind::Mods {
        let Some(profile) = profile else {
            return Err("Mod search requires an active profile context".to_string());
        };
        if profile.loader == ProfileLoader::Vanilla {
            return Ok(Vec::new());
        }
    }

    let facets = format!("[[\"project_type:{}\"]]", kind.project_type_facet());

    let mut url = url::Url::parse("https://api.modrinth.com/v2/search")
        .map_err(|e| format!("failed to build Modrinth search URL: {e}"))?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("query", query);
        qp.append_pair("index", "relevance");
        qp.append_pair("limit", "24");
        qp.append_pair("facets", &facets);
    }

    let response = Client::new()
        .get(url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("Modrinth search request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Modrinth search failed: {e}"))?;

    let payload = response
        .json::<ModrinthSearchResponse>()
        .map_err(|e| format!("Modrinth search decode failed: {e}"))?;

    let mut results: Vec<DiscoveryCardData> = payload
        .hits
        .into_iter()
        .map(|hit| DiscoveryCardData {
            project_id: hit.project_id,
            title: hit.title,
            description: hit
                .description
                .unwrap_or_else(|| "No description available.".to_string()),
        })
        .collect();

    if kind == DiscoveryKind::Mods {
        let profile = profile.expect("validated above");
        let mut filtered = Vec::new();
        for item in results {
            if fetch_compatible_modrinth_version_for_project(&item.project_id, profile)?.is_some() {
                filtered.push(item);
            }
        }
        results = filtered;
    }

    Ok(results)
}

fn profile_loader_to_modrinth_loader(profile: &LauncherProfile) -> Option<&'static str> {
    match profile.loader {
        ProfileLoader::Vanilla => None,
        ProfileLoader::Fabric => Some("fabric"),
        ProfileLoader::Quilt => Some("quilt"),
        ProfileLoader::Forge => Some("forge"),
        ProfileLoader::NeoForge => Some("neoforge"),
    }
}

fn profile_loader_modrinth_loaders(profile: &LauncherProfile) -> Vec<&'static str> {
    match profile.loader {
        ProfileLoader::Vanilla => Vec::new(),
        ProfileLoader::Fabric => vec!["fabric"],
        ProfileLoader::Quilt => vec!["quilt", "fabric"],
        ProfileLoader::Forge => vec!["forge"],
        ProfileLoader::NeoForge => vec!["neoforge", "forge"],
    }
}

fn is_modrinth_version_compatible(version: &ModrinthVersion, profile: &LauncherProfile) -> bool {
    if !version.game_versions.iter().any(|v| v == &profile.version_id) {
        return false;
    }

    let required_loaders = profile_loader_modrinth_loaders(profile);
    if required_loaders.is_empty() {
        return false;
    }

    version.loaders.iter().any(|loader| {
        required_loaders
            .iter()
            .any(|required| loader.eq_ignore_ascii_case(required))
    })
}

fn sha1_file_hex(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed reading '{}': {e}", path.display()))?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn fetch_modrinth_version_by_hash(hash: &str) -> Result<Option<ModrinthVersion>, String> {
    let mut url = url::Url::parse(&format!("https://api.modrinth.com/v2/version_file/{hash}"))
        .map_err(|e| format!("failed to build Modrinth version lookup URL: {e}"))?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("algorithm", "sha1");
    }

    let response = Client::new()
        .get(url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed querying Modrinth version by hash: {e}"))?;

    if response.status().as_u16() == 404 {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .map_err(|e| format!("Modrinth version hash lookup failed: {e}"))?;

    response
        .json::<ModrinthVersion>()
        .map(Some)
        .map_err(|e| format!("failed decoding Modrinth version hash payload: {e}"))
}

fn fetch_modrinth_compatible_update_for_hash(
    hash: &str,
    profile: &LauncherProfile,
) -> Result<Option<ModrinthVersion>, String> {
    let loaders = serde_json::to_string(&profile_loader_modrinth_loaders(profile))
        .map_err(|e| format!("failed serializing loader filter: {e}"))?;
    let versions = serde_json::to_string(&vec![profile.version_id.clone()])
        .map_err(|e| format!("failed serializing game version filter: {e}"))?;

    let mut url = url::Url::parse(&format!(
        "https://api.modrinth.com/v2/version_file/{hash}/update"
    ))
    .map_err(|e| format!("failed to build Modrinth update lookup URL: {e}"))?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("algorithm", "sha1");
        qp.append_pair("loaders", &loaders);
        qp.append_pair("game_versions", &versions);
    }

    let response = Client::new()
        .get(url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed querying Modrinth update by hash: {e}"))?;

    if response.status().as_u16() == 404 {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .map_err(|e| format!("Modrinth update hash lookup failed: {e}"))?;

    response
        .json::<ModrinthVersion>()
        .map(Some)
        .map_err(|e| format!("failed decoding Modrinth update hash payload: {e}"))
}

fn fetch_compatible_modrinth_version_for_project(
    project_id: &str,
    profile: &LauncherProfile,
) -> Result<Option<ModrinthVersion>, String> {
    let versions_url = url::Url::parse(&format!(
        "https://api.modrinth.com/v2/project/{project_id}/version"
    ))
    .map_err(|e| format!("failed to build Modrinth project versions URL: {e}"))?;

    let versions = Client::new()
        .get(versions_url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed requesting Modrinth project versions: {e}"))?
        .error_for_status()
        .map_err(|e| format!("failed fetching Modrinth project versions: {e}"))?
        .json::<Vec<ModrinthVersion>>()
        .map_err(|e| format!("failed decoding Modrinth project versions: {e}"))?;

    Ok(versions
        .into_iter()
        .find(|version| is_modrinth_version_compatible(version, profile) && !version.files.is_empty()))
}

fn disable_mod_file(path: &Path) -> Result<PathBuf, String> {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err(format!("failed resolving filename for '{}'", path.display()));
    };

    let disabled_path = path.with_file_name(format!("{file_name}.disabled"));
    fs::rename(path, &disabled_path).map_err(|e| {
        format!(
            "failed disabling incompatible mod '{}': {e}",
            path.display()
        )
    })?;
    Ok(disabled_path)
}

fn apply_modrinth_update_to_mod_path(
    update: ModrinthVersion,
    existing_path: &Path,
) -> Result<PathBuf, String> {
    let file = update
        .files
        .iter()
        .find(|f| f.primary.unwrap_or(false))
        .or_else(|| update.files.first())
        .ok_or_else(|| "Modrinth update payload had no downloadable files".to_string())?;

    let target_path = existing_path
        .parent()
        .ok_or_else(|| format!("failed resolving parent dir for '{}'", existing_path.display()))?
        .join(sanitize_component_for_path(&file.filename));

    let bytes = Client::new()
        .get(&file.url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed downloading compatible mod update: {e}"))?
        .error_for_status()
        .map_err(|e| format!("compatible mod update download failed: {e}"))?
        .bytes()
        .map_err(|e| format!("failed reading compatible mod update bytes: {e}"))?;

    fs::write(&target_path, bytes)
        .map_err(|e| format!("failed writing '{}': {e}", target_path.display()))?;

    if target_path != existing_path {
        fs::remove_file(existing_path).map_err(|e| {
            format!(
                "failed removing old incompatible mod '{}': {e}",
                existing_path.display()
            )
        })?;
    }

    Ok(target_path)
}

fn auto_repair_profile_mods(
    profile: &LauncherProfile,
    tx: &mpsc::Sender<LauncherMessage>,
) -> Result<ModRepairSummary, String> {
    let mods_dir = profile_content_dir(profile, DiscoveryKind::Mods)?;
    let mut summary = ModRepairSummary {
        checked: 0,
        updated: 0,
        disabled: 0,
        unknown: 0,
        disabled_mods: Vec::new(),
    };

    let mut enabled_mods = Vec::new();
    for entry in fs::read_dir(&mods_dir)
        .map_err(|e| format!("failed reading '{}': {e}", mods_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("failed iterating '{}': {e}", mods_dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".disabled") {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jar") {
            continue;
        }
        enabled_mods.push(path);
    }

    if enabled_mods.is_empty() {
        return Ok(summary);
    }

    let _ = tx.send(LauncherMessage::Log(format!(
        "Auto-repair: checking {} enabled mod(s) for compatibility (MC {}, loader {})",
        enabled_mods.len(),
        profile.version_id,
        profile.loader_label()
    )));

    let mut pending_disable: Vec<PathBuf> = Vec::new();

    for mod_path in enabled_mods {
        summary.checked += 1;
        let mod_name = mod_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();

        let hash = sha1_file_hex(&mod_path)?;
        let current_version = fetch_modrinth_version_by_hash(&hash)?;

        match current_version {
            None => {
                summary.unknown += 1;
                let _ = tx.send(LauncherMessage::Log(format!(
                    "Auto-repair: '{}' not found on Modrinth hash index; leaving enabled",
                    mod_name
                )));
            }
            Some(version) if is_modrinth_version_compatible(&version, profile) => {
                let _ = tx.send(LauncherMessage::Log(format!(
                    "Auto-repair: '{}' is compatible",
                    mod_name
                )));
            }
            Some(version) => {
                let _ = tx.send(LauncherMessage::Log(format!(
                    "Auto-repair: '{}' is incompatible, searching same-mod compatible replacement...",
                    mod_name
                )));

                let replacement = fetch_modrinth_compatible_update_for_hash(&hash, profile)?
                    .or_else(|| {
                        version.project_id.as_deref().and_then(|project_id| {
                            fetch_compatible_modrinth_version_for_project(project_id, profile).ok().flatten()
                        })
                    });

                if let Some(update) = replacement {
                    let updated_path = apply_modrinth_update_to_mod_path(update, &mod_path)?;
                    summary.updated += 1;
                    let updated_name = updated_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>");
                    let _ = tx.send(LauncherMessage::Log(format!(
                        "Auto-repair: replaced '{}' with compatible '{}'",
                        mod_name, updated_name
                    )));
                } else {
                    let _ = tx.send(LauncherMessage::Log(format!(
                        "Auto-repair: no compatible replacement found for '{}'; will disable after retrieval phase",
                        mod_name
                    )));
                    pending_disable.push(mod_path);
                }
            }
        }
    }

    for mod_path in pending_disable {
        let disabled_path = disable_mod_file(&mod_path)?;
        summary.disabled += 1;
        let disabled_name = disabled_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        summary.disabled_mods.push(disabled_name.clone());
        let _ = tx.send(LauncherMessage::Log(format!(
            "Auto-repair: disabled incompatible mod '{}'",
            disabled_name
        )));
    }

    Ok(summary)
}

fn install_modrinth_project(
    kind: DiscoveryKind,
    project: &DiscoveryCardData,
    profile: &LauncherProfile,
) -> Result<PathBuf, String> {
    if kind == DiscoveryKind::Mods && profile.loader == ProfileLoader::Vanilla {
        return Err(
            "Mods require a modded profile loader (Fabric, Quilt, Forge, or NeoForge).".to_string(),
        );
    }

    let versions_url = url::Url::parse(&format!(
        "https://api.modrinth.com/v2/project/{}/version",
        project.project_id
    ))
    .map_err(|e| format!("failed to build Modrinth versions URL: {e}"))?;

    let versions = Client::new()
        .get(versions_url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed requesting Modrinth versions: {e}"))?
        .error_for_status()
        .map_err(|e| format!("failed fetching compatible Modrinth version: {e}"))?
        .json::<Vec<ModrinthVersion>>()
        .map_err(|e| format!("failed decoding Modrinth versions: {e}"))?;

    let wanted_game_version = &profile.version_id;
    let wanted_loader = profile_loader_to_modrinth_loader(profile);

    let version = versions
        .into_iter()
        .find(|v| {
            let game_match = v.game_versions.iter().any(|gv| gv == wanted_game_version);
            if !game_match {
                return false;
            }

            if kind == DiscoveryKind::Mods {
                if let Some(loader) = wanted_loader {
                    return v.loaders.iter().any(|l| l.eq_ignore_ascii_case(loader));
                }
            }

            true
        })
        .ok_or_else(|| {
            if kind == DiscoveryKind::Mods {
                format!(
                    "No compatible mod files found for profile '{}' (MC {}, loader {}). Client-side filter checked game_versions + loaders.",
                    profile.name,
                    profile.version_id,
                    profile.loader_label()
                )
            } else {
                format!(
                    "No compatible shader files found for profile '{}' (MC {}). Client-side filter checked game_versions.",
                    profile.name,
                    profile.version_id
                )
            }
        })?;

    let file = version
        .files
        .iter()
        .find(|f| f.primary.unwrap_or(false))
        .or_else(|| version.files.first())
        .ok_or_else(|| "Modrinth version had no downloadable files".to_string())?;

    let install_dir = profile_content_dir(profile, kind)?;
    let target_path = install_dir.join(sanitize_component_for_path(&file.filename));
    let bytes = Client::new()
        .get(&file.url)
        .header("User-Agent", "LucentLauncher/0.1 (Modrinth integration)")
        .send()
        .map_err(|e| format!("failed downloading file: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download failed: {e}"))?
        .bytes()
        .map_err(|e| format!("failed reading download bytes: {e}"))?;

    fs::write(&target_path, bytes)
        .map_err(|e| format!("failed writing '{}': {e}", target_path.display()))?;

    Ok(target_path)
}

fn maven_artifact_relative_path(name: &str) -> Option<PathBuf> {
    let mut parts = name.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    let classifier = parts.next();

    if parts.next().is_some() {
        return None;
    }

    let mut file_name = format!("{artifact}-{version}");
    if let Some(classifier) = classifier {
        file_name.push('-');
        file_name.push_str(classifier);
    }
    file_name.push_str(".jar");

    let group_path = group.replace('.', "/");
    Some(
        PathBuf::from(group_path)
            .join(artifact)
            .join(version)
            .join(file_name),
    )
}

fn classpath_dedupe_key(entry: &str) -> String {
    let normalized = entry.replace('\\', "/");

    if let Some(idx) = normalized.find("/libraries/") {
        let rel = &normalized[idx + "/libraries/".len()..];
        let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() >= 4 {
            let artifact = parts[parts.len() - 3];
            let version = parts[parts.len() - 2];
            let file = parts[parts.len() - 1];
            let group = parts[..parts.len() - 3].join(".");

            let stem = file.strip_suffix(".jar").unwrap_or(file);
            let prefix = format!("{artifact}-{version}");
            let classifier = if stem == prefix {
                ""
            } else if let Some(rest) = stem.strip_prefix(&(prefix + "-")) {
                rest
            } else {
                ""
            };

            return format!("maven:{group}:{artifact}:{classifier}");
        }
    }

    format!("path:{normalized}")
}

fn dedupe_classpath_string(classpath: &str) -> (String, usize) {
    let separator = if classpath.contains(';') { ';' } else { ':' };
    let entries: Vec<String> = classpath
        .split(separator)
        .filter(|e| !e.is_empty())
        .map(|e| e.to_string())
        .collect();

    if entries.is_empty() {
        return (classpath.to_string(), 0);
    }

    let mut last_seen_index: HashMap<String, usize> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        last_seen_index.insert(classpath_dedupe_key(entry), idx);
    }

    let mut kept = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.iter().enumerate() {
        let key = classpath_dedupe_key(entry);
        if last_seen_index.get(&key) == Some(&idx) {
            kept.push(entry.clone());
        }
    }

    let removed = entries.len().saturating_sub(kept.len());
    (kept.join(&separator.to_string()), removed)
}

fn dedupe_launch_classpath(args: &mut [String]) -> usize {
    for idx in 0..args.len().saturating_sub(1) {
        if args[idx] == "-cp" || args[idx] == "-classpath" {
            let (deduped, removed) = dedupe_classpath_string(&args[idx + 1]);
            args[idx + 1] = deduped;
            return removed;
        }
    }
    0
}

fn resolve_latest_forge_version_for_minecraft(minecraft_version: &str) -> Result<String, String> {
    let versions = mc_launcher_core::loader::forge::list_forge_versions()
        .map_err(|e| format!("failed listing Forge versions: {e}"))?;

    versions
        .into_iter()
        .filter(|v| v.starts_with(&format!("{minecraft_version}-")))
        .last()
        .ok_or_else(|| {
            format!(
                "No Forge versions found for Minecraft {}. Choose Exact loader version or a different Minecraft version.",
                minecraft_version
            )
        })
}

fn ensure_launcher_profiles_json(minecraft_dir: &Path) -> Result<(), String> {
    let launcher_profiles_path = minecraft_dir.join("launcher_profiles.json");
    if launcher_profiles_path.exists() {
        return Ok(());
    }

    let scaffold = serde_json::json!({
        "profiles": {
            "Lucent": {
                "name": "Lucent",
                "type": "custom"
            }
        },
        "selectedProfile": "Lucent",
        "clientToken": "00000000-0000-0000-0000-000000000000",
        "authenticationDatabase": {},
        "settings": {},
        "launcherVersion": {
            "name": "Lucent Launcher",
            "format": 21
        }
    });

    let content = serde_json::to_string_pretty(&scaffold)
        .map_err(|e| format!("failed serializing launcher_profiles.json scaffold: {e}"))?;
    fs::write(&launcher_profiles_path, content)
        .map_err(|e| format!("failed writing '{}': {e}", launcher_profiles_path.display()))
}

fn install_forge_profile_with_java(
    launcher: &mc_launcher_core::launcher::Launcher,
    minecraft_version: &str,
    forge_version: &str,
    java_path_raw: &str,
    tx: &mpsc::Sender<LauncherMessage>,
) -> Result<String, String> {
    use mc_launcher_core::install::InstallRequest;

    launcher
        .install(InstallRequest::vanilla(minecraft_version))
        .map_err(|e| {
            format!(
                "failed installing base Minecraft {}: {e:?}",
                minecraft_version
            )
        })?;

    ensure_launcher_profiles_json(launcher.minecraft_dir())?;
    let _ = tx.send(LauncherMessage::Log(
        "Ensured launcher_profiles.json exists for Forge installer compatibility".to_string(),
    ));

    let installer_url = mc_launcher_core::loader::forge::installer_url(forge_version);
    let installer_dir = launcher.minecraft_dir().join("installers");
    fs::create_dir_all(&installer_dir).map_err(|e| {
        format!(
            "failed creating installer dir '{}': {e}",
            installer_dir.display()
        )
    })?;

    let installer_path = installer_dir.join(format!("forge-{forge_version}-installer.jar"));
    if !installer_path.exists() {
        let _ = tx.send(LauncherMessage::Log(format!(
            "Downloading Forge installer: {}",
            installer_url
        )));

        let response = Client::new()
            .get(&installer_url)
            .send()
            .map_err(|e| format!("failed requesting Forge installer: {e}"))?
            .error_for_status()
            .map_err(|e| format!("failed downloading Forge installer: {e}"))?;

        let bytes = response
            .bytes()
            .map_err(|e| format!("failed reading Forge installer bytes: {e}"))?;

        fs::write(&installer_path, bytes).map_err(|e| {
            format!(
                "failed writing Forge installer '{}': {e}",
                installer_path.display()
            )
        })?;
    }

    let java_executable = if !java_path_raw.is_empty() && java_path_raw != "/path/to/binary" {
        PathBuf::from(java_path_raw)
    } else {
        PathBuf::from("java")
    };

    let _ = tx.send(LauncherMessage::Log(format!(
        "Running Forge installer with Java '{}'",
        java_executable.display()
    )));

    if minecraft_version.starts_with("1.8.") || minecraft_version.starts_with("1.7.") {
        let _ = tx.send(LauncherMessage::Log(
            "[forge-installer] Legacy Forge detected; Java 8 is typically required for 1.7/1.8 installs"
                .to_string(),
        ));
    }

    match std::process::Command::new(&java_executable)
        .arg("-version")
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let _ = tx.send(LauncherMessage::Log(format!("[forge-java] {line}")));
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                let _ = tx.send(LauncherMessage::Log(format!("[forge-java] {line}")));
            }
        }
        Err(e) => {
            let _ = tx.send(LauncherMessage::Log(format!(
                "[forge-java] failed running java -version: {e}"
            )));
        }
    }

    let mut cmd = std::process::Command::new(&java_executable);
    cmd.arg("-jar")
        .arg(&installer_path)
        .arg("--installClient")
        .arg(launcher.minecraft_dir())
        .current_dir(launcher.minecraft_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed spawning Forge installer: {e}"))?;

    let out_tx = tx.clone();
    let out_reader = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = out_tx.send(LauncherMessage::Log(format!("[forge-installer] {line}")));
            }
        })
    });

    let err_tx = tx.clone();
    let err_reader = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let _ = err_tx.send(LauncherMessage::Log(format!(
                    "[forge-installer:stderr] {line}"
                )));
            }
        })
    });

    let status = child
        .wait()
        .map_err(|e| format!("failed waiting for Forge installer: {e}"))?;

    if let Some(h) = out_reader {
        let _ = h.join();
    }
    if let Some(h) = err_reader {
        let _ = h.join();
    }

    if !status.success() {
        let java_hint =
            if minecraft_version.starts_with("1.8.") || minecraft_version.starts_with("1.7.") {
                " For older Forge (1.7/1.8), configure Java 8 in Profile Editor > Runtime Settings."
            } else {
                ""
            };
        return Err(format!(
            "Forge installer exited with status {}.{}",
            status, java_hint
        ));
    }

    mc_launcher_core::loader::forge::forge_installed_version_id(forge_version)
        .map_err(|e| format!("failed resolving installed Forge version id: {e}"))
}

fn ensure_maven_fallback_libraries_present(
    version: &mc_launcher_core::core::version::VersionJson,
    minecraft_dir: &Path,
    tx: &mpsc::Sender<LauncherMessage>,
) -> Result<(), String> {
    let client = Client::new();

    for lib in &version.libraries {
        if lib
            .downloads
            .as_ref()
            .and_then(|d| d.artifact.as_ref())
            .is_some()
        {
            continue;
        }
        if lib.natives.is_some() {
            continue;
        }

        let rel_path = match maven_artifact_relative_path(&lib.name) {
            Some(path) => path,
            None => continue,
        };
        let destination = minecraft_dir.join("libraries").join(&rel_path);
        if destination.exists() {
            continue;
        }

        let base_url = lib
            .url
            .clone()
            .unwrap_or_else(|| "https://libraries.minecraft.net/".to_string());
        let base_url = if base_url.ends_with('/') {
            base_url
        } else {
            format!("{base_url}/")
        };
        let rel_url = rel_path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let url = format!("{base_url}{rel_url}");

        let _ = tx.send(LauncherMessage::Log(format!(
            "Downloading missing library: {}",
            lib.name
        )));

        let response = client
            .get(&url)
            .send()
            .map_err(|e| format!("failed requesting library '{}': {e}", lib.name))?
            .error_for_status()
            .map_err(|e| format!("failed downloading library '{}': {e}", lib.name))?;

        let bytes = response
            .bytes()
            .map_err(|e| format!("failed reading library '{}': {e}", lib.name))?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed creating library directory '{}' for '{}': {e}",
                    parent.display(),
                    lib.name
                )
            })?;
        }

        fs::write(&destination, &bytes).map_err(|e| {
            format!(
                "failed writing library '{}' to '{}': {e}",
                lib.name,
                destination.display()
            )
        })?;
    }

    Ok(())
}

fn parse_auth_code_and_state(callback_url: &str) -> Result<(String, Option<String>), String> {
    let parsed =
        url::Url::parse(callback_url).map_err(|e| format!("failed parsing callback URL: {e}"))?;
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;

    for (k, v) in parsed.query_pairs() {
        if k == "code" {
            code = Some(v.to_string());
        } else if k == "state" {
            state = Some(v.to_string());
        }
    }

    match code {
        Some(c) if !c.is_empty() => Ok((c, state)),
        _ => Err("oauth callback did not contain an authorization code".to_string()),
    }
}

fn authenticate_with_minecraft_access_token(
    userhash: &str,
    xsts_token: &str,
) -> Result<String, String> {
    let parameters = serde_json::json!({
        "identityToken": format!("XBL3.0 x={};{}", userhash, xsts_token),
    });

    let response = Client::new()
        .post("https://api.minecraftservices.com/authentication/login_with_xbox")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&parameters)
        .send()
        .map_err(|e| format!("minecraft xbox login request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|e| format!("failed reading minecraft xbox login response: {e}"))?;

    let json: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid minecraft xbox login JSON ({status}): {e}; body={body}"))?;

    if !status.is_success() {
        return Err(format!("minecraft xbox login failed ({status}): {json}"));
    }

    match json.get("access_token").and_then(Value::as_str) {
        Some(token) if !token.is_empty() => Ok(token.to_string()),
        _ => Err(format!(
            "minecraft xbox login succeeded but access_token missing: {json}"
        )),
    }
}

fn fetch_minecraft_profile(access_token: &str) -> Result<(String, String), String> {
    let response = Client::new()
        .get("https://api.minecraftservices.com/minecraft/profile")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("minecraft profile request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|e| format!("failed reading minecraft profile response: {e}"))?;

    let json: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid minecraft profile JSON ({status}): {e}; body={body}"))?;

    if !status.is_success() {
        return Err(format!(
            "minecraft profile lookup failed ({status}): {json}"
        ));
    }

    let id = json
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("minecraft profile missing id: {json}"))?
        .to_string();
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("minecraft profile missing name: {json}"))?
        .to_string();

    Ok((id, name))
}

fn complete_microsoft_login_resilient(
    client_id: &str,
    redirect_uri: &str,
    auth_code: &str,
    code_verifier: &str,
) -> Result<(String, String, String, String), String> {
    use mc_launcher_core::auth::microsoft_account;

    let token_request = microsoft_account::get_authorization_token(
        client_id,
        None,
        redirect_uri,
        auth_code,
        Some(code_verifier),
    )
    .map_err(|e| format!("authorization token exchange failed: {e}"))?;

    if let Some(err) = token_request.error {
        return Err(format!(
            "authorization token exchange returned error: {err}"
        ));
    }

    let xbl = microsoft_account::authenticate_with_xbl(&token_request.access_token)
        .map_err(|e| format!("xbl authentication failed: {e}"))?;
    let userhash = xbl
        .display_claims
        .xui
        .first()
        .map(|x| x.uhs.clone())
        .ok_or_else(|| "xbl response missing user hash".to_string())?;

    let xsts = microsoft_account::authenticate_with_xsts(&xbl.token)
        .map_err(|e| format!("xsts authentication failed: {e}"))?;

    let mc_access_token = authenticate_with_minecraft_access_token(&userhash, &xsts.token)?;
    let (uuid, username) = fetch_minecraft_profile(&mc_access_token)?;

    Ok((username, uuid, mc_access_token, token_request.refresh_token))
}

fn complete_microsoft_refresh_resilient(
    client_id: &str,
    refresh_token: &str,
) -> Result<(String, String, String, String), String> {
    use mc_launcher_core::auth::microsoft_account;

    let token_request =
        microsoft_account::refresh_authorization_token(client_id, None, refresh_token)
            .map_err(|e| format!("authorization token refresh failed: {e}"))?;

    if let Some(err) = token_request.error {
        return Err(format!("authorization token refresh returned error: {err}"));
    }

    let xbl = microsoft_account::authenticate_with_xbl(&token_request.access_token)
        .map_err(|e| format!("xbl authentication failed: {e}"))?;
    let userhash = xbl
        .display_claims
        .xui
        .first()
        .map(|x| x.uhs.clone())
        .ok_or_else(|| "xbl response missing user hash".to_string())?;

    let xsts = microsoft_account::authenticate_with_xsts(&xbl.token)
        .map_err(|e| format!("xsts authentication failed: {e}"))?;

    let mc_access_token = authenticate_with_minecraft_access_token(&userhash, &xsts.token)?;
    let (uuid, username) = fetch_minecraft_profile(&mc_access_token)?;

    Ok((username, uuid, mc_access_token, token_request.refresh_token))
}

fn parse_loopback_port_from_redirect_uri(uri: &str) -> Option<u16> {
    let rest = uri
        .strip_prefix("http://localhost:")
        .or_else(|| uri.strip_prefix("http://127.0.0.1:"))?;

    let port_text = rest.split('/').next().unwrap_or_default();
    port_text.parse::<u16>().ok()
}

fn resolve_microsoft_redirect_uri() -> Result<String, String> {
    match std::env::var(MS_REDIRECT_URI_ENV) {
        Ok(value) if !value.trim().is_empty() => {
            let uri = value.trim().to_string();
            if parse_loopback_port_from_redirect_uri(&uri).is_none() {
                return Err(format!(
                    "{MS_REDIRECT_URI_ENV} must be an http loopback URL with an explicit port, e.g. http://localhost:53682/callback"
                ));
            }
            Ok(uri)
        }
        _ => Ok(DEFAULT_MS_REDIRECT_URI.to_string()),
    }
}

fn wait_for_oauth_redirect(listener: TcpListener, timeout: Duration) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("failed to configure oauth callback listener: {e}"))?;

    let port = listener
        .local_addr()
        .map_err(|e| format!("failed reading callback listener addr: {e}"))?
        .port();

    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err("Timed out waiting for Microsoft sign-in callback".to_string());
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut req = [0_u8; 4096];
                let n = stream
                    .read(&mut req)
                    .map_err(|e| format!("failed reading oauth callback request: {e}"))?;

                if n == 0 {
                    continue;
                }

                let request = String::from_utf8_lossy(&req[..n]);
                let first_line = request.lines().next().unwrap_or_default();
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();

                if method != "GET" || path.is_empty() {
                    continue;
                }

                let body = "<html><body><h2>Login complete</h2><p>You can close this tab and return to Lucent Launcher.</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();

                return Ok(format!("http://127.0.0.1:{port}{path}"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("oauth callback listener failed: {e}")),
        }
    }
}

fn spawn_microsoft_login_flow(tx: mpsc::Sender<LauncherMessage>, client_id: String) {
    thread::spawn(move || {
        use mc_launcher_core::auth::microsoft_account;

        let _ = tx.send(LauncherMessage::StatusUpdate(
            "Starting Microsoft sign-in...".to_string(),
        ));

        let redirect_uri = match resolve_microsoft_redirect_uri() {
            Ok(uri) => uri,
            Err(e) => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(e));
                return;
            }
        };

        let port = match parse_loopback_port_from_redirect_uri(&redirect_uri) {
            Some(port) => port,
            None => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(format!(
                    "Invalid redirect URI format: {redirect_uri}"
                )));
                return;
            }
        };

        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => listener,
            Err(e) => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(format!(
                    "Failed to open local callback port: {e}"
                )));
                return;
            }
        };

        let (login_url, state, code_verifier) =
            microsoft_account::get_secure_login_data(&client_id, &redirect_uri, None);

        let _ = tx.send(LauncherMessage::Log(format!(
            "Open browser for Microsoft login. Redirect URI: {redirect_uri}"
        )));
        let _ = tx.send(LauncherMessage::OpenUrl(login_url.clone()));
        let _ = tx.send(LauncherMessage::StatusUpdate(
            "Waiting for Microsoft login in browser...".to_string(),
        ));

        let callback_url = match wait_for_oauth_redirect(listener, Duration::from_secs(180)) {
            Ok(url) => url,
            Err(e) => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(e));
                return;
            }
        };

        let (auth_code, callback_state) = match parse_auth_code_and_state(&callback_url) {
            Ok(parsed) => parsed,
            Err(e) => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(format!(
                    "Failed parsing OAuth callback: {e}"
                )));
                return;
            }
        };

        if callback_state.as_deref() != Some(state.as_str()) {
            let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(
                "OAuth callback state mismatch; aborting login for safety".to_string(),
            ));
            return;
        }

        match complete_microsoft_login_resilient(
            &client_id,
            &redirect_uri,
            &auth_code,
            &code_verifier,
        ) {
            Ok((username, uuid, access_token, refresh_token)) => {
                if let Err(e) = save_refresh_token(&refresh_token) {
                    let _ = tx.send(LauncherMessage::Log(format!(
                        "[WARN] Signed in, but failed to store refresh token in keyring: {e}"
                    )));
                }

                let _ = tx.send(LauncherMessage::MicrosoftAuthSuccess {
                    username,
                    uuid,
                    access_token,
                    refresh_token,
                });
            }
            Err(e) => {
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(format!(
                    "Microsoft login failed: {e}"
                )));
            }
        }
    });
}

fn spawn_microsoft_refresh_flow(
    tx: mpsc::Sender<LauncherMessage>,
    client_id: String,
    refresh_token: String,
) {
    thread::spawn(move || {
        let _ = tx.send(LauncherMessage::StatusUpdate(
            "Restoring Microsoft session...".to_string(),
        ));

        match complete_microsoft_refresh_resilient(&client_id, &refresh_token) {
            Ok((username, uuid, access_token, refresh_token)) => {
                if let Err(e) = save_refresh_token(&refresh_token) {
                    let _ = tx.send(LauncherMessage::Log(format!(
                        "[WARN] Failed to rotate stored refresh token: {e}"
                    )));
                }

                let _ = tx.send(LauncherMessage::MicrosoftAuthSuccess {
                    username,
                    uuid,
                    access_token,
                    refresh_token,
                });
            }
            Err(e) => {
                let _ = clear_refresh_token();
                let _ = tx.send(LauncherMessage::Log(
                    "Stored Microsoft session is invalid; please sign in again.".to_string(),
                ));
                let _ = tx.send(LauncherMessage::MicrosoftAuthFailed(format!(
                    "Microsoft session restore failed: {e}"
                )));
            }
        }
    });
}

fn build_ui(app: &Application) {
    let builder = gtk::Builder::new();
    builder
        .add_from_resource(UI_RESOURCE_PATH)
        .expect("Failed to load launcher UI file");

    // --- Core Widget References ---
    let toolbar_view: adw::ToolbarView = builder.object("toolbar_view").unwrap();
    let view_stack: Stack = builder.object("view_stack").unwrap();
    let dropdown_profile_launch: DropDown = builder.object("dropdown_profile_launch").unwrap();
    let dropdown_profile_editor: DropDown = builder.object("dropdown_profile_editor").unwrap();
    let dropdown_profile_version: DropDown = builder.object("dropdown_profile_version").unwrap();
    let dropdown_profile_loader: DropDown = builder.object("dropdown_profile_loader").unwrap();
    let dropdown_profile_loader_version_mode: DropDown = builder
        .object("dropdown_profile_loader_version_mode")
        .unwrap();
    let dropdown_profile_color_mode: DropDown =
        builder.object("dropdown_profile_color_mode").unwrap();
    let btn_play: Button = builder.object("btn_play").unwrap();
    let btn_switch_user: Button = builder.object("btn_switch_user").unwrap();
    let btn_login: Button = builder.object("btn_login").unwrap();
    let btn_login_microsoft: Button = builder.object("btn_login_microsoft").unwrap();
    let btn_profile_create: Button = builder.object("btn_profile_create").unwrap();
    let btn_profile_save: Button = builder.object("btn_profile_save").unwrap();
    let btn_profile_delete: Button = builder.object("btn_profile_delete").unwrap();
    let row_login_username: EntryRow = builder.object("row_login_username").unwrap();
    let row_profile_name: EntryRow = builder.object("row_profile_name").unwrap();
    let row_profile_loader_version_exact: EntryRow =
        builder.object("row_profile_loader_version_exact").unwrap();
    let row_profile_color_hex: EntryRow = builder.object("row_profile_color_hex").unwrap();
    let row_account_status: ActionRow = builder.object("row_account_status").unwrap();
    let row_java_binary: EntryRow = builder.object("row_java_binary").unwrap();
    let dropdown_profile_runtime_java_policy: DropDown = builder
        .object("dropdown_profile_runtime_java_policy")
        .unwrap();
    let row_profile_runtime_memory_mb: EntryRow =
        builder.object("row_profile_runtime_memory_mb").unwrap();
    let row_profile_runtime_jvm_args: EntryRow =
        builder.object("row_profile_runtime_jvm_args").unwrap();
    let lbl_welcome_user: Label = builder.object("lbl_welcome_user").unwrap();
    let lbl_ready_status: Label = builder.object("lbl_ready_status").unwrap();
    let text_view: TextView = builder.object("text_view").unwrap();
    let flow_home_profiles: FlowBox = builder.object("flow_home_profiles").unwrap();
    let progress_bar: ProgressBar = builder.object("progress_bar").unwrap();
    let bottom_deck: gtk::Widget = builder.object("bottom_deck").unwrap();

    let btn_profile_manage_mods: Button = builder.object("btn_profile_manage_mods").unwrap();
    let btn_profile_manage_shaders: Button = builder.object("btn_profile_manage_shaders").unwrap();

    let sheet_profile_mods: BottomSheet = builder.object("sheet_profile_mods").unwrap();
    let entry_profile_mods_search: GtkEntry = builder.object("entry_profile_mods_search").unwrap();
    let btn_profile_mods_search: Button = builder.object("btn_profile_mods_search").unwrap();
    let lbl_profile_mods_installed_status: Label =
        builder.object("lbl_profile_mods_installed_status").unwrap();
    let lbl_profile_mods_results_status: Label =
        builder.object("lbl_profile_mods_results_status").unwrap();
    let flow_profile_mods_installed: FlowBox =
        builder.object("flow_profile_mods_installed").unwrap();
    let flow_profile_mods_results: FlowBox = builder.object("flow_profile_mods_results").unwrap();
    let btn_profile_mods_sheet_cancel: Button =
        builder.object("btn_profile_mods_sheet_cancel").unwrap();
    let btn_profile_mods_sheet_install: Button =
        builder.object("btn_profile_mods_sheet_install").unwrap();

    let sheet_profile_shaders: BottomSheet = builder.object("sheet_profile_shaders").unwrap();
    let entry_profile_shaders_search: GtkEntry =
        builder.object("entry_profile_shaders_search").unwrap();
    let btn_profile_shaders_search: Button = builder.object("btn_profile_shaders_search").unwrap();
    let lbl_profile_shaders_installed_status: Label = builder
        .object("lbl_profile_shaders_installed_status")
        .unwrap();
    let lbl_profile_shaders_results_status: Label = builder
        .object("lbl_profile_shaders_results_status")
        .unwrap();
    let flow_profile_shaders_installed: FlowBox =
        builder.object("flow_profile_shaders_installed").unwrap();
    let flow_profile_shaders_results: FlowBox =
        builder.object("flow_profile_shaders_results").unwrap();
    let scroll_profile_shaders_installed: gtk::ScrolledWindow =
        builder.object("scroll_profile_shaders_installed").unwrap();
    let scroll_profile_shaders_results: gtk::ScrolledWindow =
        builder.object("scroll_profile_shaders_results").unwrap();
    let scroll_profile_mods_installed: gtk::ScrolledWindow =
        builder.object("scroll_profile_mods_installed").unwrap();
    let scroll_profile_mods_results: gtk::ScrolledWindow =
        builder.object("scroll_profile_mods_results").unwrap();
    let btn_profile_shaders_sheet_cancel: Button =
        builder.object("btn_profile_shaders_sheet_cancel").unwrap();
    let btn_profile_shaders_sheet_install: Button =
        builder.object("btn_profile_shaders_sheet_install").unwrap();

    // --- 3. Standard thread Channel ---
    let (tx, rx) = mpsc::channel::<LauncherMessage>();

    // --- 1. Populate Version/Profile Functionality ---
    let loading_model = StringList::new(&["Loading versions…"]);
    dropdown_profile_version.set_model(Some(&loading_model));
    dropdown_profile_version.set_selected(0);
    dropdown_profile_version.set_sensitive(false);

    let loader_model = StringList::new(&["Vanilla", "Fabric", "Quilt", "Forge", "NeoForge"]);
    dropdown_profile_loader.set_model(Some(&loader_model));
    dropdown_profile_loader.set_selected(0);
    dropdown_profile_loader.set_sensitive(false);

    let loader_version_mode_model = StringList::new(&["Latest Stable", "Latest", "Exact"]);
    dropdown_profile_loader_version_mode.set_model(Some(&loader_version_mode_model));
    dropdown_profile_loader_version_mode.set_selected(0);
    dropdown_profile_loader_version_mode.set_sensitive(false);
    row_profile_loader_version_exact.set_sensitive(false);

    let color_mode_model = StringList::new(&["Automatic", "Custom"]);
    dropdown_profile_color_mode.set_model(Some(&color_mode_model));
    dropdown_profile_color_mode.set_selected(0);
    dropdown_profile_color_mode.set_sensitive(false);
    row_profile_color_hex.set_sensitive(false);

    let runtime_java_policy_model = StringList::new(&["Auto", "Never"]);
    dropdown_profile_runtime_java_policy.set_model(Some(&runtime_java_policy_model));
    dropdown_profile_runtime_java_policy.set_selected(0);
    dropdown_profile_runtime_java_policy.set_sensitive(false);
    row_java_binary.set_sensitive(false);
    row_profile_runtime_memory_mb.set_sensitive(false);
    row_profile_runtime_jvm_args.set_sensitive(false);

    let profile_loading_model = StringList::new(&["No profiles"]);
    dropdown_profile_launch.set_model(Some(&profile_loading_model));
    dropdown_profile_launch.set_selected(0);
    dropdown_profile_launch.set_sensitive(false);
    dropdown_profile_editor.set_model(Some(&profile_loading_model));
    dropdown_profile_editor.set_selected(0);
    dropdown_profile_editor.set_sensitive(false);

    btn_profile_manage_mods.set_sensitive(false);
    btn_profile_manage_shaders.set_sensitive(false);

    btn_profile_create.set_sensitive(false);
    btn_profile_save.set_sensitive(false);
    btn_profile_delete.set_sensitive(false);

    let bottom_deck_for_mods_open = bottom_deck.clone();
    let shaders_sheet_for_mods_open = sheet_profile_shaders.clone();
    let mods_installed_scroll_for_resize = scroll_profile_mods_installed.clone();
    let mods_results_scroll_for_resize = scroll_profile_mods_results.clone();
    sheet_profile_mods.connect_open_notify(move |mods_sheet| {
        let any_open = mods_sheet.is_open() || shaders_sheet_for_mods_open.is_open();
        bottom_deck_for_mods_open.set_visible(!any_open);

        if mods_sheet.is_open() {
            if let Some(root) = mods_sheet.root() {
                if let Ok(window) = root.downcast::<gtk::ApplicationWindow>() {
                    let h = window.height().max(1);
                    let target_sheet_height = ((h as f32) * 0.80).round() as i32;
                    let tab_scroll_height = (target_sheet_height - 170).clamp(220, 900);
                    mods_installed_scroll_for_resize.set_max_content_height(tab_scroll_height);
                    mods_installed_scroll_for_resize.set_min_content_height(tab_scroll_height);
                    mods_results_scroll_for_resize.set_max_content_height(tab_scroll_height);
                    mods_results_scroll_for_resize.set_min_content_height(tab_scroll_height);
                }
            }
        }
    });

    let bottom_deck_for_shaders_open = bottom_deck.clone();
    let mods_sheet_for_shaders_open = sheet_profile_mods.clone();
    let shaders_installed_scroll_for_resize = scroll_profile_shaders_installed.clone();
    let shaders_results_scroll_for_resize = scroll_profile_shaders_results.clone();
    sheet_profile_shaders.connect_open_notify(move |shaders_sheet| {
        let any_open = shaders_sheet.is_open() || mods_sheet_for_shaders_open.is_open();
        bottom_deck_for_shaders_open.set_visible(!any_open);

        if shaders_sheet.is_open() {
            if let Some(root) = shaders_sheet.root() {
                if let Ok(window) = root.downcast::<gtk::ApplicationWindow>() {
                    let h = window.height().max(1);
                    let target_sheet_height = ((h as f32) * 0.80).round() as i32;
                    let tab_scroll_height = (target_sheet_height - 170).clamp(220, 900);
                    shaders_installed_scroll_for_resize.set_max_content_height(tab_scroll_height);
                    shaders_installed_scroll_for_resize.set_min_content_height(tab_scroll_height);
                    shaders_results_scroll_for_resize.set_max_content_height(tab_scroll_height);
                    shaders_results_scroll_for_resize.set_min_content_height(tab_scroll_height);
                }
            }
        }
    });

    let versions_tx = tx.clone();
    thread::spawn(move || {
        use mc_launcher_core::utils::get_version_list;

        match get_version_list() {
            Ok(list) => {
                let releases: Vec<String> = list
                    .into_iter()
                    .filter(|v| v.r#type == "release")
                    .map(|v| v.id)
                    .collect();
                let _ = versions_tx.send(LauncherMessage::VersionsLoaded(releases));
            }
            Err(e) => {
                let _ = versions_tx.send(LauncherMessage::TaskFailed(format!(
                    "Failed to query version manifest: {e}"
                )));
            }
        }
    });

    // --- 2. Account/Login/Profile State ---
    let current_session: Rc<RefCell<Option<Session>>> = Rc::new(RefCell::new(None));
    let available_versions: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let profiles: Rc<RefCell<Vec<LauncherProfile>>> = Rc::new(RefCell::new(Vec::new()));

    let mods_results: Rc<RefCell<Vec<DiscoveryCardData>>> = Rc::new(RefCell::new(Vec::new()));
    let shaders_results: Rc<RefCell<Vec<DiscoveryCardData>>> = Rc::new(RefCell::new(Vec::new()));
    let mods_installed: Rc<RefCell<Vec<InstalledContentEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let shaders_installed: Rc<RefCell<Vec<InstalledContentEntry>>> =
        Rc::new(RefCell::new(Vec::new()));
    let selected_mod_projects: Rc<RefCell<HashMap<String, DiscoveryCardData>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let selected_shader_projects: Rc<RefCell<HashMap<String, DiscoveryCardData>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let mods_results_render = mods_results.clone();
    let flow_mods_results_render = flow_profile_mods_results.clone();
    let selected_mod_projects_render = selected_mod_projects.clone();
    let render_mods_cards_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let render_mods_cards_holder_for_init = render_mods_cards_holder.clone();
    let render_mods_cards: Rc<dyn Fn()> = Rc::new(move || {
        while let Some(child) = flow_mods_results_render.first_child() {
            flow_mods_results_render.remove(&child);
        }

        let mut selected_items: Vec<DiscoveryCardData> =
            selected_mod_projects_render.borrow().values().cloned().collect();
        selected_items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));

        if !selected_items.is_empty() {
            let selected_header = Label::new(Some("Selected for install"));
            selected_header.set_xalign(0.0);
            selected_header.add_css_class("heading");
            flow_mods_results_render.insert(&selected_header, -1);

            for item in selected_items {
                let frame = Frame::new(None);
                frame.set_hexpand(true);
                frame.set_valign(gtk::Align::Start);
                ensure_discovery_card_css(&frame);

                let row = GtkBox::new(Orientation::Horizontal, 10);
                row.set_margin_top(12);
                row.set_margin_bottom(12);
                row.set_margin_start(12);
                row.set_margin_end(12);

                let check = CheckButton::new();
                check.set_active(true);
                check.set_size_request(22, 22);
                check.set_valign(gtk::Align::Center);
                check.add_css_class("discovery-select-check");

                let text_col = GtkBox::new(Orientation::Vertical, 6);
                text_col.set_hexpand(true);

                let title = Label::new(Some(&item.title));
                title.set_xalign(0.0);
                title.set_wrap(false);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                title.add_css_class("heading");

                let subtitle = Label::new(Some(&item.description));
                subtitle.set_xalign(0.0);
                subtitle.set_wrap(true);
                subtitle.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                subtitle.set_max_width_chars(56);
                subtitle.add_css_class("dim-label");

                let selected_mod_projects_toggle = selected_mod_projects_render.clone();
                let render_mods_cards_toggle = render_mods_cards_holder_for_init.clone();
                let project_id = item.project_id.clone();
                check.connect_toggled(move |c| {
                    if !c.is_active() {
                        selected_mod_projects_toggle.borrow_mut().remove(&project_id);
                        if let Some(render) = render_mods_cards_toggle.borrow().as_ref() {
                            (render)();
                        }
                    }
                });

                text_col.append(&title);
                text_col.append(&subtitle);
                row.append(&check);
                row.append(&text_col);
                frame.set_child(Some(&row));

                flow_mods_results_render.insert(&frame, -1);
            }
        }

        let search_results_header = if selected_mod_projects_render.borrow().is_empty() {
            "Search results"
        } else {
            "Search results (unchecked items)"
        };
        let header = Label::new(Some(search_results_header));
        header.set_xalign(0.0);
        header.add_css_class("dim-label");
        flow_mods_results_render.insert(&header, -1);

        let selected_ids: HashSet<String> = selected_mod_projects_render
            .borrow()
            .keys()
            .cloned()
            .collect();

        for item in mods_results_render.borrow().iter() {
            if selected_ids.contains(&item.project_id) {
                continue;
            }

            let frame = Frame::new(None);
            frame.set_hexpand(true);
            frame.set_valign(gtk::Align::Start);
            ensure_discovery_card_css(&frame);

            let row = GtkBox::new(Orientation::Horizontal, 10);
            row.set_margin_top(12);
            row.set_margin_bottom(12);
            row.set_margin_start(12);
            row.set_margin_end(12);

            let check = CheckButton::new();
            check.set_active(false);
            check.set_size_request(22, 22);
            check.set_valign(gtk::Align::Center);
            check.add_css_class("discovery-select-check");

            let text_col = GtkBox::new(Orientation::Vertical, 6);
            text_col.set_hexpand(true);

            let title = Label::new(Some(&item.title));
            title.set_xalign(0.0);
            title.set_wrap(false);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("heading");

            let subtitle = Label::new(Some(&item.description));
            subtitle.set_xalign(0.0);
            subtitle.set_wrap(true);
            subtitle.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            subtitle.set_max_width_chars(56);
            subtitle.add_css_class("dim-label");

            let selected_mod_projects_toggle = selected_mod_projects_render.clone();
            let render_mods_cards_toggle = render_mods_cards_holder_for_init.clone();
            let item_for_toggle = item.clone();
            check.connect_toggled(move |c| {
                if c.is_active() {
                    selected_mod_projects_toggle
                        .borrow_mut()
                        .insert(item_for_toggle.project_id.clone(), item_for_toggle.clone());
                    if let Some(render) = render_mods_cards_toggle.borrow().as_ref() {
                        (render)();
                    }
                }
            });

            text_col.append(&title);
            text_col.append(&subtitle);
            row.append(&check);
            row.append(&text_col);
            frame.set_child(Some(&row));

            flow_mods_results_render.insert(&frame, -1);
        }
    });
    *render_mods_cards_holder.borrow_mut() = Some(render_mods_cards.clone());

    let shaders_results_render = shaders_results.clone();
    let flow_shaders_results_render = flow_profile_shaders_results.clone();
    let selected_shader_projects_render = selected_shader_projects.clone();
    let render_shaders_cards_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let render_shaders_cards_holder_for_init = render_shaders_cards_holder.clone();
    let render_shaders_cards: Rc<dyn Fn()> = Rc::new(move || {
        while let Some(child) = flow_shaders_results_render.first_child() {
            flow_shaders_results_render.remove(&child);
        }

        let mut selected_items: Vec<DiscoveryCardData> =
            selected_shader_projects_render.borrow().values().cloned().collect();
        selected_items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));

        if !selected_items.is_empty() {
            let selected_header = Label::new(Some("Selected for install"));
            selected_header.set_xalign(0.0);
            selected_header.add_css_class("heading");
            flow_shaders_results_render.insert(&selected_header, -1);

            for item in selected_items {
                let frame = Frame::new(None);
                frame.set_hexpand(true);
                frame.set_valign(gtk::Align::Start);
                ensure_discovery_card_css(&frame);

                let row = GtkBox::new(Orientation::Horizontal, 10);
                row.set_margin_top(12);
                row.set_margin_bottom(12);
                row.set_margin_start(12);
                row.set_margin_end(12);

                let check = CheckButton::new();
                check.set_active(true);
                check.set_size_request(22, 22);
                check.set_valign(gtk::Align::Center);
                check.add_css_class("discovery-select-check");

                let text_col = GtkBox::new(Orientation::Vertical, 6);
                text_col.set_hexpand(true);

                let title = Label::new(Some(&item.title));
                title.set_xalign(0.0);
                title.set_wrap(false);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                title.add_css_class("heading");

                let subtitle = Label::new(Some(&item.description));
                subtitle.set_xalign(0.0);
                subtitle.set_wrap(true);
                subtitle.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                subtitle.set_max_width_chars(56);
                subtitle.add_css_class("dim-label");

                let selected_shader_projects_toggle = selected_shader_projects_render.clone();
                let render_shaders_cards_toggle = render_shaders_cards_holder_for_init.clone();
                let project_id = item.project_id.clone();
                check.connect_toggled(move |c| {
                    if !c.is_active() {
                        selected_shader_projects_toggle.borrow_mut().remove(&project_id);
                        if let Some(render) = render_shaders_cards_toggle.borrow().as_ref() {
                            (render)();
                        }
                    }
                });

                text_col.append(&title);
                text_col.append(&subtitle);
                row.append(&check);
                row.append(&text_col);
                frame.set_child(Some(&row));

                flow_shaders_results_render.insert(&frame, -1);
            }
        }

        let search_results_header = if selected_shader_projects_render.borrow().is_empty() {
            "Search results"
        } else {
            "Search results (unchecked items)"
        };
        let header = Label::new(Some(search_results_header));
        header.set_xalign(0.0);
        header.add_css_class("dim-label");
        flow_shaders_results_render.insert(&header, -1);

        let selected_ids: HashSet<String> = selected_shader_projects_render
            .borrow()
            .keys()
            .cloned()
            .collect();

        for item in shaders_results_render.borrow().iter() {
            if selected_ids.contains(&item.project_id) {
                continue;
            }

            let frame = Frame::new(None);
            frame.set_hexpand(true);
            frame.set_valign(gtk::Align::Start);
            ensure_discovery_card_css(&frame);

            let row = GtkBox::new(Orientation::Horizontal, 10);
            row.set_margin_top(12);
            row.set_margin_bottom(12);
            row.set_margin_start(12);
            row.set_margin_end(12);

            let check = CheckButton::new();
            check.set_active(false);
            check.set_size_request(22, 22);
            check.set_valign(gtk::Align::Center);
            check.add_css_class("discovery-select-check");

            let text_col = GtkBox::new(Orientation::Vertical, 6);
            text_col.set_hexpand(true);

            let title = Label::new(Some(&item.title));
            title.set_xalign(0.0);
            title.set_wrap(false);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("heading");

            let subtitle = Label::new(Some(&item.description));
            subtitle.set_xalign(0.0);
            subtitle.set_wrap(true);
            subtitle.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            subtitle.set_max_width_chars(56);
            subtitle.add_css_class("dim-label");

            let selected_shader_projects_toggle = selected_shader_projects_render.clone();
            let render_shaders_cards_toggle = render_shaders_cards_holder_for_init.clone();
            let item_for_toggle = item.clone();
            check.connect_toggled(move |c| {
                if c.is_active() {
                    selected_shader_projects_toggle
                        .borrow_mut()
                        .insert(item_for_toggle.project_id.clone(), item_for_toggle.clone());
                    if let Some(render) = render_shaders_cards_toggle.borrow().as_ref() {
                        (render)();
                    }
                }
            });

            text_col.append(&title);
            text_col.append(&subtitle);
            row.append(&check);
            row.append(&text_col);
            frame.set_child(Some(&row));

            flow_shaders_results_render.insert(&frame, -1);
        }
    });
    *render_shaders_cards_holder.borrow_mut() = Some(render_shaders_cards.clone());

    let mods_installed_render = mods_installed.clone();
    let flow_mods_installed_render = flow_profile_mods_installed.clone();
    let profiles_for_mods_installed_render = profiles.clone();
    let profile_editor_for_mods_installed_render = dropdown_profile_editor.clone();
    let tx_mods_installed_render = tx.clone();
    let render_mods_installed_cards: Rc<dyn Fn()> = Rc::new(move || {
        while let Some(child) = flow_mods_installed_render.first_child() {
            flow_mods_installed_render.remove(&child);
        }

        let profile_idx = profile_editor_for_mods_installed_render.selected() as usize;
        let profile = profiles_for_mods_installed_render
            .borrow()
            .get(profile_idx)
            .cloned();

        for item in mods_installed_render.borrow().iter() {
            let frame = Frame::new(None);
            frame.set_hexpand(true);
            frame.set_valign(gtk::Align::Start);
            ensure_discovery_card_css(&frame);

            let row = GtkBox::new(Orientation::Horizontal, 10);
            row.set_margin_top(10);
            row.set_margin_bottom(10);
            row.set_margin_start(10);
            row.set_margin_end(10);

            let name = Label::new(Some(&item.display_name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_wrap(false);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let state = Switch::new();
            state.set_active(item.enabled);
            state.set_hexpand(false);
            state.set_vexpand(false);
            state.set_halign(gtk::Align::End);
            state.set_valign(gtk::Align::Center);

            let del = Button::builder().icon_name("user-trash-symbolic").build();
            del.set_halign(gtk::Align::End);
            del.set_valign(gtk::Align::Center);
            del.add_css_class("flat");

            if let Some(profile) = profile.clone() {
                let profile_for_toggle = profile.clone();
                let tx_toggle = tx_mods_installed_render.clone();
                let file_name = item.file_name.clone();
                state.connect_state_set(move |sw, value| {
                    if let Err(e) = set_profile_content_enabled(
                        &profile_for_toggle,
                        DiscoveryKind::Mods,
                        &file_name,
                        value,
                    ) {
                        let _ = tx_toggle.send(LauncherMessage::TaskFailed(e));
                        sw.set_active(!value);
                    } else {
                        sw.set_active(value);
                        let _ = tx_toggle.send(LauncherMessage::DiscoveryInstalledChanged(
                            DiscoveryKind::Mods,
                        ));
                    }
                    glib::Propagation::Stop
                });

                let profile_for_delete = profile.clone();
                let tx_delete = tx_mods_installed_render.clone();
                let file_name = item.file_name.clone();
                del.connect_clicked(move |_| {
                    if let Err(e) =
                        delete_profile_content(&profile_for_delete, DiscoveryKind::Mods, &file_name)
                    {
                        let _ = tx_delete.send(LauncherMessage::TaskFailed(e));
                    } else {
                        let _ = tx_delete.send(LauncherMessage::DiscoveryInstalledChanged(
                            DiscoveryKind::Mods,
                        ));
                    }
                });
            } else {
                state.set_sensitive(false);
                del.set_sensitive(false);
            }

            row.append(&name);
            row.append(&state);
            row.append(&del);
            frame.set_child(Some(&row));
            flow_mods_installed_render.insert(&frame, -1);
        }
    });

    let shaders_installed_render = shaders_installed.clone();
    let flow_shaders_installed_render = flow_profile_shaders_installed.clone();
    let profiles_for_shaders_installed_render = profiles.clone();
    let profile_editor_for_shaders_installed_render = dropdown_profile_editor.clone();
    let tx_shaders_installed_render = tx.clone();
    let render_shaders_installed_cards: Rc<dyn Fn()> = Rc::new(move || {
        while let Some(child) = flow_shaders_installed_render.first_child() {
            flow_shaders_installed_render.remove(&child);
        }

        let profile_idx = profile_editor_for_shaders_installed_render.selected() as usize;
        let profile = profiles_for_shaders_installed_render
            .borrow()
            .get(profile_idx)
            .cloned();

        for item in shaders_installed_render.borrow().iter() {
            let frame = Frame::new(None);
            frame.set_hexpand(true);
            frame.set_valign(gtk::Align::Start);
            ensure_discovery_card_css(&frame);

            let row = GtkBox::new(Orientation::Horizontal, 10);
            row.set_margin_top(10);
            row.set_margin_bottom(10);
            row.set_margin_start(10);
            row.set_margin_end(10);

            let name = Label::new(Some(&item.display_name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_wrap(false);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let state = Switch::new();
            state.set_active(item.enabled);
            state.set_hexpand(false);
            state.set_vexpand(false);
            state.set_halign(gtk::Align::End);
            state.set_valign(gtk::Align::Center);

            let del = Button::builder().icon_name("user-trash-symbolic").build();
            del.set_halign(gtk::Align::End);
            del.set_valign(gtk::Align::Center);
            del.add_css_class("flat");

            if let Some(profile) = profile.clone() {
                let profile_for_toggle = profile.clone();
                let tx_toggle = tx_shaders_installed_render.clone();
                let file_name = item.file_name.clone();
                state.connect_state_set(move |sw, value| {
                    if let Err(e) = set_profile_content_enabled(
                        &profile_for_toggle,
                        DiscoveryKind::Shaders,
                        &file_name,
                        value,
                    ) {
                        let _ = tx_toggle.send(LauncherMessage::TaskFailed(e));
                        sw.set_active(!value);
                    } else {
                        sw.set_active(value);
                        let _ = tx_toggle.send(LauncherMessage::DiscoveryInstalledChanged(
                            DiscoveryKind::Shaders,
                        ));
                    }
                    glib::Propagation::Stop
                });

                let profile_for_delete = profile.clone();
                let tx_delete = tx_shaders_installed_render.clone();
                let file_name = item.file_name.clone();
                del.connect_clicked(move |_| {
                    if let Err(e) = delete_profile_content(
                        &profile_for_delete,
                        DiscoveryKind::Shaders,
                        &file_name,
                    ) {
                        let _ = tx_delete.send(LauncherMessage::TaskFailed(e));
                    } else {
                        let _ = tx_delete.send(LauncherMessage::DiscoveryInstalledChanged(
                            DiscoveryKind::Shaders,
                        ));
                    }
                });
            } else {
                state.set_sensitive(false);
                del.set_sensitive(false);
            }

            row.append(&name);
            row.append(&state);
            row.append(&del);
            frame.set_child(Some(&row));
            flow_shaders_installed_render.insert(&frame, -1);
        }
    });

    let profiles_for_home_cards = profiles.clone();
    let current_session_for_home_cards = current_session.clone();
    let flow_home_profiles_render = flow_home_profiles.clone();
    let dropdown_profile_launch_for_cards = dropdown_profile_launch.clone();
    let dropdown_profile_editor_for_cards = dropdown_profile_editor.clone();
    let view_stack_for_cards = view_stack.clone();
    let btn_play_for_cards = btn_play.clone();
    let render_home_cards: Rc<dyn Fn()> = Rc::new(move || {
        while let Some(child) = flow_home_profiles_render.first_child() {
            flow_home_profiles_render.remove(&child);
        }

        let signed_in = current_session_for_home_cards.borrow().is_some();
        let profile_list = profiles_for_home_cards.borrow();

        for (idx, profile) in profile_list.iter().enumerate() {
            let card_frame = Frame::new(None);
            card_frame.set_size_request(250, 132);
            card_frame.set_valign(gtk::Align::Start);
            apply_glass_card_gradient(&card_frame, profile);

            let card_root = GtkBox::new(Orientation::Vertical, 8);
            card_root.set_margin_top(10);
            card_root.set_margin_bottom(10);
            card_root.set_margin_start(10);
            card_root.set_margin_end(10);

            let info_box = GtkBox::new(Orientation::Vertical, 4);
            info_box.set_vexpand(true);

            let title = Label::new(Some(&profile.name));
            title.set_xalign(0.0);
            title.set_wrap(false);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("heading");

            let subtitle = Label::new(Some(&format!(
                "{} • {} • {}",
                profile.version_id,
                profile.loader_label(),
                profile.loader_version_label()
            )));
            subtitle.set_xalign(0.0);
            subtitle.add_css_class("dim-label");
            subtitle.set_wrap(false);
            subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let vertical_spacer = GtkBox::new(Orientation::Vertical, 0);
            vertical_spacer.set_vexpand(true);

            let bottom_row = GtkBox::new(Orientation::Horizontal, 6);
            bottom_row.set_hexpand(true);
            let spacer = GtkBox::new(Orientation::Horizontal, 0);
            spacer.set_hexpand(true);

            let btn_card_play = Button::builder()
                .icon_name("media-playback-start-symbolic")
                .build();
            btn_card_play.add_css_class("flat");
            btn_card_play.add_css_class("circular");
            btn_card_play.set_tooltip_text(Some("Launch profile"));
            btn_card_play.set_halign(gtk::Align::End);
            btn_card_play.set_valign(gtk::Align::End);
            btn_card_play.set_sensitive(signed_in);

            let launch_dropdown = dropdown_profile_launch_for_cards.clone();
            let launch_btn = btn_play_for_cards.clone();
            btn_card_play.connect_clicked(move |_| {
                launch_dropdown.set_selected(idx as u32);
                launch_btn.emit_clicked();
            });

            bottom_row.append(&spacer);
            bottom_row.append(&btn_card_play);

            let card_click = GestureClick::new();
            let editor_dropdown = dropdown_profile_editor_for_cards.clone();
            let view_stack = view_stack_for_cards.clone();
            let play_btn_for_hit_test = btn_card_play.clone();
            let card_frame_for_hit_test = card_frame.clone();
            card_click.connect_pressed(move |_, _, x, y| {
                if let Some(bounds) = play_btn_for_hit_test.compute_bounds(&card_frame_for_hit_test)
                {
                    let point = gtk::graphene::Point::new(x as f32, y as f32);
                    if bounds.contains_point(&point) {
                        return;
                    }
                }

                editor_dropdown.set_selected(idx as u32);
                view_stack.set_visible_child_name("page_profile");
            });
            card_frame.add_controller(card_click);

            title.add_controller({
                let click = GestureClick::new();
                let editor_dropdown = dropdown_profile_editor_for_cards.clone();
                let view_stack = view_stack_for_cards.clone();
                click.connect_pressed(move |_, _, _, _| {
                    editor_dropdown.set_selected(idx as u32);
                    view_stack.set_visible_child_name("page_profile");
                });
                click
            });
            subtitle.add_controller({
                let click = GestureClick::new();
                let editor_dropdown = dropdown_profile_editor_for_cards.clone();
                let view_stack = view_stack_for_cards.clone();
                click.connect_pressed(move |_, _, _, _| {
                    editor_dropdown.set_selected(idx as u32);
                    view_stack.set_visible_child_name("page_profile");
                });
                click
            });

            info_box.append(&title);
            info_box.append(&subtitle);
            info_box.append(&vertical_spacer);
            info_box.append(&bottom_row);

            card_root.append(&info_box);
            card_frame.set_child(Some(&card_root));

            flow_home_profiles_render.insert(&card_frame, -1);
            if let Some(parent) = card_frame.parent() {
                if let Ok(flow_child) = parent.downcast::<gtk::FlowBoxChild>() {
                    flow_child.set_halign(gtk::Align::Start);
                    flow_child.set_valign(gtk::Align::Start);
                }
            }
        }
    });

    match load_profiles_from_disk() {
        Ok(saved_profiles) => {
            if !saved_profiles.is_empty() {
                *profiles.borrow_mut() = saved_profiles;
                let buffer = text_view.buffer();
                buffer.insert_at_cursor("Loaded profiles from profiles.json\n");
            }
        }
        Err(e) => {
            let buffer = text_view.buffer();
            buffer.insert_at_cursor(&format!("[WARN] Failed loading profiles from disk: {e}\n"));
        }
    }

    (render_home_cards)();

    let profiles_for_refresh = profiles.clone();
    let versions_for_refresh = available_versions.clone();
    let launch_dropdown_refresh = dropdown_profile_launch.clone();
    let editor_dropdown_refresh = dropdown_profile_editor.clone();
    let btn_profile_manage_mods_refresh = btn_profile_manage_mods.clone();
    let btn_profile_manage_shaders_refresh = btn_profile_manage_shaders.clone();
    let version_dropdown_refresh = dropdown_profile_version.clone();
    let loader_dropdown_refresh = dropdown_profile_loader.clone();
    let loader_version_mode_dropdown_refresh = dropdown_profile_loader_version_mode.clone();
    let color_mode_dropdown_refresh = dropdown_profile_color_mode.clone();
    let row_profile_name_refresh = row_profile_name.clone();
    let row_loader_version_exact_refresh = row_profile_loader_version_exact.clone();
    let row_profile_color_hex_refresh = row_profile_color_hex.clone();
    let dropdown_profile_runtime_java_policy_refresh = dropdown_profile_runtime_java_policy.clone();
    let row_java_binary_refresh = row_java_binary.clone();
    let row_profile_runtime_memory_mb_refresh = row_profile_runtime_memory_mb.clone();
    let row_profile_runtime_jvm_args_refresh = row_profile_runtime_jvm_args.clone();
    let btn_profile_create_refresh = btn_profile_create.clone();
    let btn_profile_save_refresh = btn_profile_save.clone();
    let btn_profile_delete_refresh = btn_profile_delete.clone();
    let current_session_refresh = current_session.clone();
    let btn_play_refresh = btn_play.clone();

    let render_home_cards_for_refresh = render_home_cards.clone();
    let refresh_profile_models: Rc<dyn Fn(Option<String>)> =
        Rc::new(move |preferred_profile_name| {
            let profile_list = profiles_for_refresh.borrow();
            let version_list = versions_for_refresh.borrow();

            if profile_list.is_empty() {
                let no_profiles = StringList::new(&["No profiles"]);
                launch_dropdown_refresh.set_model(Some(&no_profiles));
                launch_dropdown_refresh.set_selected(0);
                launch_dropdown_refresh.set_sensitive(false);

                let no_profiles_editor = StringList::new(&["No profiles"]);
                editor_dropdown_refresh.set_model(Some(&no_profiles_editor));
                editor_dropdown_refresh.set_selected(0);
                editor_dropdown_refresh.set_sensitive(false);

                btn_profile_manage_mods_refresh.set_sensitive(false);
                btn_profile_manage_shaders_refresh.set_sensitive(false);

                version_dropdown_refresh.set_sensitive(false);
                loader_dropdown_refresh.set_sensitive(false);
                loader_version_mode_dropdown_refresh.set_sensitive(false);
                color_mode_dropdown_refresh.set_sensitive(false);
                color_mode_dropdown_refresh.set_selected(0);
                row_loader_version_exact_refresh.set_sensitive(false);
                row_loader_version_exact_refresh.set_text("");
                row_profile_color_hex_refresh.set_sensitive(false);
                row_profile_color_hex_refresh.set_text("");
                dropdown_profile_runtime_java_policy_refresh.set_sensitive(false);
                dropdown_profile_runtime_java_policy_refresh.set_selected(0);
                row_java_binary_refresh.set_sensitive(false);
                row_java_binary_refresh.set_text("");
                row_profile_runtime_memory_mb_refresh.set_sensitive(false);
                row_profile_runtime_memory_mb_refresh.set_text("");
                row_profile_runtime_jvm_args_refresh.set_sensitive(false);
                row_profile_runtime_jvm_args_refresh.set_text("");

                btn_profile_create_refresh.set_sensitive(!version_list.is_empty());
                btn_profile_save_refresh.set_sensitive(false);
                btn_profile_delete_refresh.set_sensitive(false);
                btn_play_refresh.set_sensitive(false);
                (render_home_cards_for_refresh)();
                return;
            }

            let names: Vec<&str> = profile_list.iter().map(|p| p.name.as_str()).collect();
            let launch_model = StringList::new(&names);
            let editor_model = StringList::new(&names);
            launch_dropdown_refresh.set_model(Some(&launch_model));
            editor_dropdown_refresh.set_model(Some(&editor_model));
            launch_dropdown_refresh.set_sensitive(true);
            editor_dropdown_refresh.set_sensitive(true);

            let selected_idx = preferred_profile_name
                .as_deref()
                .and_then(|name| profile_list.iter().position(|p| p.name == name))
                .unwrap_or(0);
            let selected_idx = selected_idx.min(profile_list.len().saturating_sub(1));

            launch_dropdown_refresh.set_selected(selected_idx as u32);
            editor_dropdown_refresh.set_selected(selected_idx as u32);

            if let Some(selected_profile) = profile_list.get(selected_idx) {
                row_profile_name_refresh.set_text(&selected_profile.name);

                if !version_list.is_empty() {
                    let refs: Vec<&str> = version_list.iter().map(|v| v.as_str()).collect();
                    let version_model = StringList::new(&refs);
                    version_dropdown_refresh.set_model(Some(&version_model));
                    version_dropdown_refresh.set_sensitive(true);

                    let version_idx = version_list
                        .iter()
                        .position(|v| v == &selected_profile.version_id)
                        .unwrap_or(0);
                    version_dropdown_refresh.set_selected(version_idx as u32);
                } else {
                    version_dropdown_refresh.set_sensitive(false);
                }

                loader_dropdown_refresh.set_sensitive(true);
                color_mode_dropdown_refresh.set_sensitive(true);
                let is_vanilla = selected_profile.loader == ProfileLoader::Vanilla;
                loader_version_mode_dropdown_refresh.set_sensitive(!is_vanilla);
                loader_dropdown_refresh.set_selected(index_from_loader(&selected_profile.loader));
                loader_version_mode_dropdown_refresh
                    .set_selected(loader_version_mode_index(&selected_profile.loader_version));
                color_mode_dropdown_refresh.set_selected(match selected_profile.color_mode {
                    ProfileColorMode::Auto => 0,
                    ProfileColorMode::Custom => 1,
                });
                if selected_profile.color_mode == ProfileColorMode::Custom {
                    row_profile_color_hex_refresh.set_sensitive(true);
                    row_profile_color_hex_refresh
                        .set_text(selected_profile.color_hex.as_deref().unwrap_or("#7C5CFF"));
                } else {
                    row_profile_color_hex_refresh.set_sensitive(false);
                    row_profile_color_hex_refresh.set_text("");
                }

                match &selected_profile.loader_version {
                    ProfileLoaderVersion::Exact(exact) => {
                        row_loader_version_exact_refresh.set_sensitive(!is_vanilla);
                        row_loader_version_exact_refresh.set_text(exact);
                    }
                    _ => {
                        row_loader_version_exact_refresh.set_sensitive(false);
                        row_loader_version_exact_refresh.set_text("");
                    }
                }

                dropdown_profile_runtime_java_policy_refresh.set_sensitive(true);
                dropdown_profile_runtime_java_policy_refresh
                    .set_selected(if selected_profile.java_auto_download { 0 } else { 1 });
                row_java_binary_refresh.set_sensitive(true);
                row_java_binary_refresh
                    .set_text(selected_profile.java_binary.as_deref().unwrap_or(""));
                row_profile_runtime_memory_mb_refresh.set_sensitive(true);
                row_profile_runtime_memory_mb_refresh.set_text(
                    &selected_profile
                        .java_memory_mb
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
                row_profile_runtime_jvm_args_refresh.set_sensitive(true);
                row_profile_runtime_jvm_args_refresh
                    .set_text(selected_profile.java_args.as_deref().unwrap_or(""));
            }

            btn_profile_manage_mods_refresh.set_sensitive(true);
            btn_profile_manage_shaders_refresh.set_sensitive(true);
            btn_profile_create_refresh.set_sensitive(!version_list.is_empty());
            btn_profile_save_refresh.set_sensitive(true);
            btn_profile_delete_refresh.set_sensitive(profile_list.len() > 1);
            btn_play_refresh.set_sensitive(
                current_session_refresh.borrow().is_some() && !profile_list.is_empty(),
            );
            (render_home_cards_for_refresh)();
        });

    let profiles_for_mods_installed_refresh = profiles.clone();
    let dropdown_profile_editor_for_mods_installed_refresh = dropdown_profile_editor.clone();
    let mods_installed_refresh = mods_installed.clone();
    let lbl_profile_mods_installed_status_refresh = lbl_profile_mods_installed_status.clone();
    let render_mods_installed_cards_refresh = render_mods_installed_cards.clone();
    let refresh_mods_installed_for_selected_profile: Rc<dyn Fn()> = Rc::new(move || {
        let idx = dropdown_profile_editor_for_mods_installed_refresh.selected() as usize;
        let profile = profiles_for_mods_installed_refresh
            .borrow()
            .get(idx)
            .cloned();
        if let Some(profile) = profile {
            match list_profile_content_entries(&profile, DiscoveryKind::Mods) {
                Ok(entries) => {
                    let count = entries.len();
                    *mods_installed_refresh.borrow_mut() = entries;
                    lbl_profile_mods_installed_status_refresh
                        .set_label(&format!("Installed mods for {}: {}", profile.name, count));
                }
                Err(e) => {
                    *mods_installed_refresh.borrow_mut() = Vec::new();
                    lbl_profile_mods_installed_status_refresh
                        .set_label("Installed mods unavailable");
                    eprintln!("{e}");
                }
            }
        } else {
            *mods_installed_refresh.borrow_mut() = Vec::new();
            lbl_profile_mods_installed_status_refresh.set_label("Installed mods for this profile");
        }
        (render_mods_installed_cards_refresh)();
    });

    let profiles_for_shaders_installed_refresh = profiles.clone();
    let dropdown_profile_editor_for_shaders_installed_refresh = dropdown_profile_editor.clone();
    let shaders_installed_refresh = shaders_installed.clone();
    let lbl_profile_shaders_installed_status_refresh = lbl_profile_shaders_installed_status.clone();
    let render_shaders_installed_cards_refresh = render_shaders_installed_cards.clone();
    let refresh_shaders_installed_for_selected_profile: Rc<dyn Fn()> = Rc::new(move || {
        let idx = dropdown_profile_editor_for_shaders_installed_refresh.selected() as usize;
        let profile = profiles_for_shaders_installed_refresh
            .borrow()
            .get(idx)
            .cloned();
        if let Some(profile) = profile {
            match list_profile_content_entries(&profile, DiscoveryKind::Shaders) {
                Ok(entries) => {
                    let count = entries.len();
                    *shaders_installed_refresh.borrow_mut() = entries;
                    lbl_profile_shaders_installed_status_refresh.set_label(&format!(
                        "Installed shaders for {}: {}",
                        profile.name, count
                    ));
                }
                Err(e) => {
                    *shaders_installed_refresh.borrow_mut() = Vec::new();
                    lbl_profile_shaders_installed_status_refresh
                        .set_label("Installed shaders unavailable");
                    eprintln!("{e}");
                }
            }
        } else {
            *shaders_installed_refresh.borrow_mut() = Vec::new();
            lbl_profile_shaders_installed_status_refresh
                .set_label("Installed shaderpacks for this profile");
        }
        (render_shaders_installed_cards_refresh)();
    });

    (refresh_mods_installed_for_selected_profile)();
    (refresh_shaders_installed_for_selected_profile)();

    lbl_welcome_user.set_label("Not signed in");
    lbl_ready_status.set_label("Please log in");
    btn_play.set_sensitive(false);
    btn_switch_user.set_sensitive(false);
    view_stack.set_visible_child_name("page_account");

    // Profile editor wiring.
    let profiles_editor_sync = profiles.clone();
    let versions_editor_sync = available_versions.clone();
    let row_profile_name_sync = row_profile_name.clone();
    let dropdown_profile_version_sync = dropdown_profile_version.clone();
    let dropdown_profile_loader_sync = dropdown_profile_loader.clone();
    let dropdown_profile_loader_version_mode_sync = dropdown_profile_loader_version_mode.clone();
    let row_profile_loader_version_exact_sync = row_profile_loader_version_exact.clone();
    let dropdown_profile_runtime_java_policy_sync = dropdown_profile_runtime_java_policy.clone();
    let row_java_binary_sync = row_java_binary.clone();
    let row_profile_runtime_memory_mb_sync = row_profile_runtime_memory_mb.clone();
    let row_profile_runtime_jvm_args_sync = row_profile_runtime_jvm_args.clone();
    let dropdown_profile_launch_sync = dropdown_profile_launch.clone();
    let refresh_mods_installed_on_profile_change =
        refresh_mods_installed_for_selected_profile.clone();
    let refresh_shaders_installed_on_profile_change =
        refresh_shaders_installed_for_selected_profile.clone();
    dropdown_profile_editor.connect_selected_notify(move |editor_dropdown| {
        let idx = editor_dropdown.selected() as usize;
        dropdown_profile_launch_sync.set_selected(idx as u32);
        let profile_list = profiles_editor_sync.borrow();
        let version_list = versions_editor_sync.borrow();
        if let Some(profile) = profile_list.get(idx) {
            row_profile_name_sync.set_text(&profile.name);
            if !version_list.is_empty() {
                let version_idx = version_list
                    .iter()
                    .position(|v| v == &profile.version_id)
                    .unwrap_or(0);
                dropdown_profile_version_sync.set_selected(version_idx as u32);
            }
            dropdown_profile_loader_sync.set_selected(index_from_loader(&profile.loader));
            dropdown_profile_loader_version_mode_sync
                .set_selected(loader_version_mode_index(&profile.loader_version));
            match &profile.loader_version {
                ProfileLoaderVersion::Exact(exact) => {
                    row_profile_loader_version_exact_sync.set_sensitive(true);
                    row_profile_loader_version_exact_sync.set_text(exact);
                }
                _ => {
                    row_profile_loader_version_exact_sync.set_sensitive(false);
                    row_profile_loader_version_exact_sync.set_text("");
                }
            }
            dropdown_profile_runtime_java_policy_sync
                .set_selected(if profile.java_auto_download { 0 } else { 1 });
            row_java_binary_sync.set_text(profile.java_binary.as_deref().unwrap_or(""));
            row_profile_runtime_memory_mb_sync.set_text(
                &profile
                    .java_memory_mb
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            row_profile_runtime_jvm_args_sync.set_text(profile.java_args.as_deref().unwrap_or(""));
        }
        (refresh_mods_installed_on_profile_change)();
        (refresh_shaders_installed_on_profile_change)();
    });

    let row_loader_exact_mode = row_profile_loader_version_exact.clone();
    let loader_dropdown_for_mode = dropdown_profile_loader.clone();
    dropdown_profile_loader_version_mode.connect_selected_notify(move |dropdown| {
        let is_exact = dropdown.selected() == 2;
        let vanilla = loader_dropdown_for_mode.selected() == 0;
        row_loader_exact_mode.set_sensitive(!vanilla && is_exact);
        if !is_exact {
            row_loader_exact_mode.set_text("");
        }
    });

    let dropdown_profile_loader_version_mode_for_loader =
        dropdown_profile_loader_version_mode.clone();
    let row_profile_loader_version_exact_for_loader = row_profile_loader_version_exact.clone();
    dropdown_profile_loader.connect_selected_notify(move |dropdown| {
        let vanilla = dropdown.selected() == 0;
        dropdown_profile_loader_version_mode_for_loader.set_sensitive(!vanilla);
        if vanilla {
            row_profile_loader_version_exact_for_loader.set_sensitive(false);
            row_profile_loader_version_exact_for_loader.set_text("");
        } else {
            let is_exact = dropdown_profile_loader_version_mode_for_loader.selected() == 2;
            row_profile_loader_version_exact_for_loader.set_sensitive(is_exact);
        }
    });

    let row_profile_color_hex_for_mode = row_profile_color_hex.clone();
    dropdown_profile_color_mode.connect_selected_notify(move |dropdown| {
        let custom = dropdown.selected() == 1;
        row_profile_color_hex_for_mode.set_sensitive(custom);
        if !custom {
            row_profile_color_hex_for_mode.set_text("");
        }
    });

    let profiles_create = profiles.clone();
    let versions_create = available_versions.clone();
    let row_profile_name_create = row_profile_name.clone();
    let dropdown_profile_version_create = dropdown_profile_version.clone();
    let dropdown_profile_loader_create = dropdown_profile_loader.clone();
    let dropdown_profile_loader_version_mode_create = dropdown_profile_loader_version_mode.clone();
    let dropdown_profile_color_mode_create = dropdown_profile_color_mode.clone();
    let row_profile_loader_version_exact_create = row_profile_loader_version_exact.clone();
    let row_profile_color_hex_create = row_profile_color_hex.clone();
    let dropdown_profile_runtime_java_policy_create = dropdown_profile_runtime_java_policy.clone();
    let row_java_binary_create = row_java_binary.clone();
    let row_profile_runtime_memory_mb_create = row_profile_runtime_memory_mb.clone();
    let row_profile_runtime_jvm_args_create = row_profile_runtime_jvm_args.clone();
    let refresh_after_create = refresh_profile_models.clone();
    let refresh_mods_after_create = refresh_mods_installed_for_selected_profile.clone();
    let refresh_shaders_after_create = refresh_shaders_installed_for_selected_profile.clone();
    let lbl_status_create = lbl_ready_status.clone();
    btn_profile_create.connect_clicked(move |_| {
        let versions = versions_create.borrow();
        if versions.is_empty() {
            lbl_status_create.set_label("No versions available for profile creation");
            return;
        }

        let mut name = row_profile_name_create.text().trim().to_string();
        if name.is_empty() {
            let next = profiles_create.borrow().len() + 1;
            name = format!("Profile {next}");
        }

        let exists = profiles_create
            .borrow()
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&name));
        if exists {
            lbl_status_create.set_label("Profile name already exists");
            return;
        }

        let selected_version = dropdown_profile_version_create
            .model()
            .and_then(|m| m.downcast::<StringList>().ok())
            .and_then(|m| m.string(dropdown_profile_version_create.selected()))
            .map(|v| v.to_string())
            .or_else(|| versions.first().cloned());

        let Some(version_id) = selected_version else {
            lbl_status_create.set_label("Could not resolve selected version");
            return;
        };

        let loader = loader_from_index(dropdown_profile_loader_create.selected());
        let loader_version = if loader == ProfileLoader::Vanilla {
            ProfileLoaderVersion::LatestStable
        } else {
            match loader_version_from_mode_and_text(
                dropdown_profile_loader_version_mode_create.selected(),
                &row_profile_loader_version_exact_create.text(),
            ) {
                Some(mode) => mode,
                None => {
                    lbl_status_create.set_label("Exact loader version is required in Exact mode");
                    return;
                }
            }
        };

        let color_mode = if dropdown_profile_color_mode_create.selected() == 1 {
            ProfileColorMode::Custom
        } else {
            ProfileColorMode::Auto
        };

        let color_hex = if color_mode == ProfileColorMode::Custom {
            match normalize_hex_color(&row_profile_color_hex_create.text()) {
                Some(hex) => Some(hex),
                None => {
                    lbl_status_create.set_label("Custom color must be hex like #7C5CFF");
                    return;
                }
            }
        } else {
            None
        };

        let java_memory_mb = match parse_optional_u32(&row_profile_runtime_memory_mb_create.text()) {
            Some(v) => Some(v),
            None if row_profile_runtime_memory_mb_create.text().trim().is_empty() => None,
            None => {
                lbl_status_create.set_label("Memory limit must be an integer MB value");
                return;
            }
        };

        profiles_create.borrow_mut().push(LauncherProfile {
            name: name.clone(),
            version_id,
            loader,
            loader_version,
            color_mode,
            color_hex,
            java_binary: normalize_optional_text(&row_java_binary_create.text()),
            java_auto_download: dropdown_profile_runtime_java_policy_create.selected() == 0,
            java_memory_mb,
            java_args: normalize_optional_text(&row_profile_runtime_jvm_args_create.text()),
        });

        if let Err(e) = save_profiles_to_disk(&profiles_create.borrow()) {
            lbl_status_create.set_label("Profile created, but save failed");
            eprintln!("{e}");
        }

        (refresh_after_create)(Some(name));
        (refresh_mods_after_create)();
        (refresh_shaders_after_create)();
        lbl_status_create.set_label("Profile created");
    });

    let profiles_save = profiles.clone();
    let versions_save = available_versions.clone();
    let row_profile_name_save = row_profile_name.clone();
    let row_profile_loader_version_exact_save = row_profile_loader_version_exact.clone();
    let dropdown_profile_editor_save = dropdown_profile_editor.clone();
    let dropdown_profile_version_save = dropdown_profile_version.clone();
    let dropdown_profile_loader_save = dropdown_profile_loader.clone();
    let dropdown_profile_loader_version_mode_save = dropdown_profile_loader_version_mode.clone();
    let dropdown_profile_color_mode_save = dropdown_profile_color_mode.clone();
    let row_profile_color_hex_save = row_profile_color_hex.clone();
    let dropdown_profile_runtime_java_policy_save = dropdown_profile_runtime_java_policy.clone();
    let row_java_binary_save = row_java_binary.clone();
    let row_profile_runtime_memory_mb_save = row_profile_runtime_memory_mb.clone();
    let row_profile_runtime_jvm_args_save = row_profile_runtime_jvm_args.clone();
    let refresh_after_save = refresh_profile_models.clone();
    let refresh_mods_after_save = refresh_mods_installed_for_selected_profile.clone();
    let refresh_shaders_after_save = refresh_shaders_installed_for_selected_profile.clone();
    let lbl_status_save = lbl_ready_status.clone();
    btn_profile_save.connect_clicked(move |_| {
        let idx = dropdown_profile_editor_save.selected() as usize;
        let versions = versions_save.borrow();

        if idx >= profiles_save.borrow().len() {
            lbl_status_save.set_label("No profile selected");
            return;
        }

        let new_name = row_profile_name_save.text().trim().to_string();
        if new_name.is_empty() {
            lbl_status_save.set_label("Profile name cannot be empty");
            return;
        }

        let name_taken = profiles_save
            .borrow()
            .iter()
            .enumerate()
            .any(|(i, p)| i != idx && p.name.eq_ignore_ascii_case(&new_name));
        if name_taken {
            lbl_status_save.set_label("Another profile already uses this name");
            return;
        }

        let selected_version = dropdown_profile_version_save
            .model()
            .and_then(|m| m.downcast::<StringList>().ok())
            .and_then(|m| m.string(dropdown_profile_version_save.selected()))
            .map(|v| v.to_string())
            .or_else(|| versions.first().cloned());

        let Some(version_id) = selected_version else {
            lbl_status_save.set_label("Could not resolve selected version");
            return;
        };

        let loader = loader_from_index(dropdown_profile_loader_save.selected());
        let loader_version = if loader == ProfileLoader::Vanilla {
            ProfileLoaderVersion::LatestStable
        } else {
            match loader_version_from_mode_and_text(
                dropdown_profile_loader_version_mode_save.selected(),
                &row_profile_loader_version_exact_save.text(),
            ) {
                Some(mode) => mode,
                None => {
                    lbl_status_save.set_label("Exact loader version is required in Exact mode");
                    return;
                }
            }
        };

        let color_mode = if dropdown_profile_color_mode_save.selected() == 1 {
            ProfileColorMode::Custom
        } else {
            ProfileColorMode::Auto
        };

        let color_hex = if color_mode == ProfileColorMode::Custom {
            match normalize_hex_color(&row_profile_color_hex_save.text()) {
                Some(hex) => Some(hex),
                None => {
                    lbl_status_save.set_label("Custom color must be hex like #7C5CFF");
                    return;
                }
            }
        } else {
            None
        };

        let java_memory_mb = match parse_optional_u32(&row_profile_runtime_memory_mb_save.text()) {
            Some(v) => Some(v),
            None if row_profile_runtime_memory_mb_save.text().trim().is_empty() => None,
            None => {
                lbl_status_save.set_label("Memory limit must be an integer MB value");
                return;
            }
        };

        if let Some(profile) = profiles_save.borrow_mut().get_mut(idx) {
            profile.name = new_name.clone();
            profile.version_id = version_id;
            profile.loader = loader;
            profile.loader_version = loader_version;
            profile.color_mode = color_mode;
            profile.color_hex = color_hex;
            profile.java_binary = normalize_optional_text(&row_java_binary_save.text());
            profile.java_auto_download = dropdown_profile_runtime_java_policy_save.selected() == 0;
            profile.java_memory_mb = java_memory_mb;
            profile.java_args = normalize_optional_text(&row_profile_runtime_jvm_args_save.text());
        }

        if let Err(e) = save_profiles_to_disk(&profiles_save.borrow()) {
            lbl_status_save.set_label("Profile saved, but disk write failed");
            eprintln!("{e}");
        }

        (refresh_after_save)(Some(new_name));
        (refresh_mods_after_save)();
        (refresh_shaders_after_save)();
        lbl_status_save.set_label("Profile saved");
    });

    let profiles_delete = profiles.clone();
    let dropdown_profile_editor_delete = dropdown_profile_editor.clone();
    let refresh_after_delete = refresh_profile_models.clone();
    let refresh_mods_after_delete = refresh_mods_installed_for_selected_profile.clone();
    let refresh_shaders_after_delete = refresh_shaders_installed_for_selected_profile.clone();
    let lbl_status_delete = lbl_ready_status.clone();
    btn_profile_delete.connect_clicked(move |_| {
        let idx = dropdown_profile_editor_delete.selected() as usize;
        let mut profile_list = profiles_delete.borrow_mut();

        if profile_list.len() <= 1 {
            lbl_status_delete.set_label("At least one profile is required");
            return;
        }
        if idx >= profile_list.len() {
            lbl_status_delete.set_label("No profile selected");
            return;
        }

        profile_list.remove(idx);
        let save_res = save_profiles_to_disk(&profile_list);
        drop(profile_list);

        if let Err(e) = save_res {
            lbl_status_delete.set_label("Profile deleted, but disk write failed");
            eprintln!("{e}");
        }

        (refresh_after_delete)(None);
        (refresh_mods_after_delete)();
        (refresh_shaders_after_delete)();
        lbl_status_delete.set_label("Profile deleted");
    });

    // Offline login.
    let current_session_login = current_session.clone();
    let lbl_welcome_login = lbl_welcome_user.clone();
    let lbl_status_login = lbl_ready_status.clone();
    let row_status_login = row_account_status.clone();
    let row_username_login = row_login_username.clone();
    let btn_play_login = btn_play.clone();
    let btn_switch_login = btn_switch_user.clone();
    let view_stack_login = view_stack.clone();
    let profiles_login = profiles.clone();
    let render_home_cards_login = render_home_cards.clone();
    btn_login.connect_clicked(move |_| {
        let username = row_username_login.text().to_string();
        let username = username.trim();
        if username.is_empty() {
            row_status_login.set_subtitle("Enter a username to sign in");
            return;
        }

        *current_session_login.borrow_mut() = Some(Session::Offline {
            username: username.to_string(),
        });
        lbl_welcome_login.set_label(&format!("Hello, {username}"));
        lbl_status_login.set_label("Launcher Ready (Offline)");
        row_status_login.set_subtitle(&format!("Signed in offline as {username}"));
        btn_play_login.set_sensitive(!profiles_login.borrow().is_empty());
        btn_switch_login.set_sensitive(true);
        view_stack_login.set_visible_child_name("page_home");
        (render_home_cards_login)();
    });

    // Microsoft login.
    let row_status_ms = row_account_status.clone();
    let lbl_status_ms = lbl_ready_status.clone();
    let tx_ms = tx.clone();
    let btn_login_microsoft_for_click = btn_login_microsoft.clone();
    btn_login_microsoft.connect_clicked(move |_| {
        let client_id = match std::env::var(MS_CLIENT_ID_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                row_status_ms.set_subtitle("Microsoft login unavailable: set LUCENT_MS_CLIENT_ID");
                lbl_status_ms.set_label("Microsoft client ID missing");
                let _ = tx_ms.send(LauncherMessage::Log(
                    "Set environment variable LUCENT_MS_CLIENT_ID before launching Lucent."
                        .to_string(),
                ));
                return;
            }
        };

        btn_login_microsoft_for_click.set_sensitive(false);
        spawn_microsoft_login_flow(tx_ms.clone(), client_id);
    });

    // Restore cached Microsoft session at startup when possible.
    if let Ok(client_id) = std::env::var(MS_CLIENT_ID_ENV) {
        match load_saved_refresh_token() {
            Ok(Some(refresh_token)) => {
                let _ = tx.send(LauncherMessage::Log(
                    "Found stored Microsoft session; attempting automatic restore...".to_string(),
                ));
                btn_login_microsoft.set_sensitive(false);
                spawn_microsoft_refresh_flow(tx.clone(), client_id, refresh_token);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(LauncherMessage::Log(format!(
                    "[WARN] Could not read keyring token: {e}"
                )));
            }
        }
    }

    // Sign out.
    let current_session_switch = current_session.clone();
    let lbl_welcome_switch = lbl_welcome_user.clone();
    let lbl_status_switch = lbl_ready_status.clone();
    let row_status_switch = row_account_status.clone();
    let btn_play_switch = btn_play.clone();
    let view_stack_switch = view_stack.clone();
    let tx_switch = tx.clone();
    let btn_login_microsoft_switch = btn_login_microsoft.clone();
    let render_home_cards_switch = render_home_cards.clone();
    btn_switch_user.connect_clicked(move |btn| {
        current_session_switch.borrow_mut().take();
        lbl_welcome_switch.set_label("Not signed in");
        lbl_status_switch.set_label("Please log in");
        row_status_switch.set_subtitle("Not signed in");
        btn_play_switch.set_sensitive(false);
        btn.set_sensitive(false);
        btn_login_microsoft_switch.set_sensitive(true);
        view_stack_switch.set_visible_child_name("page_account");

        if let Err(e) = clear_refresh_token() {
            let _ = tx_switch.send(LauncherMessage::Log(format!(
                "[WARN] Failed to clear saved Microsoft token: {e}"
            )));
        }

        (render_home_cards_switch)();
    });

    // Mods/Shaders in profile editor bottom sheets.
    let sheet_profile_mods_open = sheet_profile_mods.clone();
    let mods_results_clear_on_open = mods_results.clone();
    let selected_mod_projects_open = selected_mod_projects.clone();
    let lbl_profile_mods_results_status_open = lbl_profile_mods_results_status.clone();
    let render_mods_cards_open = render_mods_cards.clone();
    let refresh_mods_installed_on_open = refresh_mods_installed_for_selected_profile.clone();
    btn_profile_manage_mods.connect_clicked(move |_| {
        mods_results_clear_on_open.borrow_mut().clear();
        selected_mod_projects_open.borrow_mut().clear();
        lbl_profile_mods_results_status_open.set_label("Search mods for this profile");
        (render_mods_cards_open)();
        (refresh_mods_installed_on_open)();
        sheet_profile_mods_open.set_open(true);
    });

    let sheet_profile_shaders_open = sheet_profile_shaders.clone();
    let shaders_results_clear_on_open = shaders_results.clone();
    let selected_shader_projects_open = selected_shader_projects.clone();
    let lbl_profile_shaders_results_status_open = lbl_profile_shaders_results_status.clone();
    let render_shaders_cards_open = render_shaders_cards.clone();
    let refresh_shaders_installed_on_open = refresh_shaders_installed_for_selected_profile.clone();
    btn_profile_manage_shaders.connect_clicked(move |_| {
        shaders_results_clear_on_open.borrow_mut().clear();
        selected_shader_projects_open.borrow_mut().clear();
        lbl_profile_shaders_results_status_open.set_label("Search shaderpacks for this profile");
        (render_shaders_cards_open)();
        (refresh_shaders_installed_on_open)();
        sheet_profile_shaders_open.set_open(true);
    });

    let tx_mods_search = tx.clone();
    let entry_mods_search_click = entry_profile_mods_search.clone();
    let lbl_mods_results_status_click = lbl_profile_mods_results_status.clone();
    let profiles_mods_search = profiles.clone();
    let profile_editor_mods_search = dropdown_profile_editor.clone();
    let mods_results_for_vanilla_clear = mods_results.clone();
    let selected_mods_for_vanilla_clear = selected_mod_projects.clone();
    let render_mods_cards_for_vanilla_clear = render_mods_cards.clone();
    btn_profile_mods_search.connect_clicked(move |_| {
        let query = entry_mods_search_click.text().trim().to_string();
        if query.is_empty() {
            lbl_mods_results_status_click.set_label("Enter a search query first");
            return;
        }

        let profile_idx = profile_editor_mods_search.selected() as usize;
        let Some(profile) = profiles_mods_search.borrow().get(profile_idx).cloned() else {
            lbl_mods_results_status_click.set_label("Select a valid profile first");
            return;
        };

        if profile.loader == ProfileLoader::Vanilla {
            mods_results_for_vanilla_clear.borrow_mut().clear();
            selected_mods_for_vanilla_clear.borrow_mut().clear();
            (render_mods_cards_for_vanilla_clear)();
            lbl_mods_results_status_click.set_label(
                "Selected profile uses Vanilla loader: mods are incompatible",
            );
            return;
        }

        lbl_mods_results_status_click.set_label("Searching Modrinth…");
        let tx = tx_mods_search.clone();
        thread::spawn(
            move || match fetch_modrinth_projects(DiscoveryKind::Mods, &query, Some(&profile)) {
                Ok(results) => {
                    let _ = tx.send(LauncherMessage::DiscoverySearchResults {
                        kind: DiscoveryKind::Mods,
                        query,
                        results,
                    });
                }
                Err(error) => {
                    let _ = tx.send(LauncherMessage::DiscoverySearchFailed {
                        kind: DiscoveryKind::Mods,
                        error,
                    });
                }
            },
        );
    });

    let btn_mods_search_activate = btn_profile_mods_search.clone();
    let lbl_mods_results_status_activate = lbl_profile_mods_results_status.clone();
    entry_profile_mods_search.connect_activate(move |entry| {
        if entry.text().trim().is_empty() {
            lbl_mods_results_status_activate.set_label("Enter a search query first");
            return;
        }
        btn_mods_search_activate.emit_clicked();
    });

    let sheet_profile_mods_cancel = sheet_profile_mods.clone();
    btn_profile_mods_sheet_cancel.connect_clicked(move |_| {
        sheet_profile_mods_cancel.set_open(false);
    });

    let profiles_mods_install = profiles.clone();
    let profile_editor_for_mods_install = dropdown_profile_editor.clone();
    let launch_dropdown_for_mods_install = dropdown_profile_launch.clone();
    let selected_mod_projects_install = selected_mod_projects.clone();
    let tx_mods_install = tx.clone();
    let sheet_profile_mods_install = sheet_profile_mods.clone();
    btn_profile_mods_sheet_install.connect_clicked(move |_| {
        let selected_projects: Vec<DiscoveryCardData> = selected_mod_projects_install
            .borrow()
            .values()
            .cloned()
            .collect();
        if selected_projects.is_empty() {
            let _ = tx_mods_install.send(LauncherMessage::TaskFailed(
                "Select one or more mods to install".to_string(),
            ));
            return;
        }

        let profile_idx = profile_editor_for_mods_install.selected() as usize;
        launch_dropdown_for_mods_install.set_selected(profile_idx as u32);
        let profile = profiles_mods_install.borrow().get(profile_idx).cloned();
        let Some(profile) = profile else {
            let _ = tx_mods_install.send(LauncherMessage::TaskFailed(
                "Select a valid profile in Profile Editor".to_string(),
            ));
            return;
        };

        sheet_profile_mods_install.set_open(false);
        let tx = tx_mods_install.clone();
        thread::spawn(move || {
            let mut installed_count = 0usize;
            for project in selected_projects {
                match install_modrinth_project(DiscoveryKind::Mods, &project, &profile) {
                    Ok(path) => {
                        installed_count += 1;
                        let _ = tx.send(LauncherMessage::DiscoveryInstallFinished {
                            kind: DiscoveryKind::Mods,
                            title: project.title,
                            target_path: path.to_string_lossy().to_string(),
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(LauncherMessage::TaskFailed(format!(
                            "Failed to install mod '{}': {}",
                            project.title, error
                        )));
                    }
                }
            }
            let _ = tx.send(LauncherMessage::StatusUpdate(format!(
                "Installed {} mod(s) for '{}'",
                installed_count, profile.name
            )));
            let _ = tx.send(LauncherMessage::DiscoveryInstalledChanged(
                DiscoveryKind::Mods,
            ));
        });
    });

    let tx_shaders_search = tx.clone();
    let entry_shaders_search_click = entry_profile_shaders_search.clone();
    let lbl_shaders_results_status_click = lbl_profile_shaders_results_status.clone();
    btn_profile_shaders_search.connect_clicked(move |_| {
        let query = entry_shaders_search_click.text().trim().to_string();
        if query.is_empty() {
            lbl_shaders_results_status_click.set_label("Enter a search query first");
            return;
        }
        lbl_shaders_results_status_click.set_label("Searching Modrinth…");
        let tx = tx_shaders_search.clone();
        thread::spawn(
            move || match fetch_modrinth_projects(DiscoveryKind::Shaders, &query, None) {
                Ok(results) => {
                    let _ = tx.send(LauncherMessage::DiscoverySearchResults {
                        kind: DiscoveryKind::Shaders,
                        query,
                        results,
                    });
                }
                Err(error) => {
                    let _ = tx.send(LauncherMessage::DiscoverySearchFailed {
                        kind: DiscoveryKind::Shaders,
                        error,
                    });
                }
            },
        );
    });

    let btn_shaders_search_activate = btn_profile_shaders_search.clone();
    let lbl_shaders_results_status_activate = lbl_profile_shaders_results_status.clone();
    entry_profile_shaders_search.connect_activate(move |entry| {
        if entry.text().trim().is_empty() {
            lbl_shaders_results_status_activate.set_label("Enter a search query first");
            return;
        }
        btn_shaders_search_activate.emit_clicked();
    });

    let sheet_profile_shaders_cancel = sheet_profile_shaders.clone();
    btn_profile_shaders_sheet_cancel.connect_clicked(move |_| {
        sheet_profile_shaders_cancel.set_open(false);
    });

    let profiles_shaders_install = profiles.clone();
    let profile_editor_for_shaders_install = dropdown_profile_editor.clone();
    let launch_dropdown_for_shaders_install = dropdown_profile_launch.clone();
    let selected_shader_projects_install = selected_shader_projects.clone();
    let tx_shaders_install = tx.clone();
    let sheet_profile_shaders_install = sheet_profile_shaders.clone();
    btn_profile_shaders_sheet_install.connect_clicked(move |_| {
        let selected_projects: Vec<DiscoveryCardData> = selected_shader_projects_install
            .borrow()
            .values()
            .cloned()
            .collect();
        if selected_projects.is_empty() {
            let _ = tx_shaders_install.send(LauncherMessage::TaskFailed(
                "Select one or more shaderpacks to install".to_string(),
            ));
            return;
        }

        let profile_idx = profile_editor_for_shaders_install.selected() as usize;
        launch_dropdown_for_shaders_install.set_selected(profile_idx as u32);
        let profile = profiles_shaders_install.borrow().get(profile_idx).cloned();
        let Some(profile) = profile else {
            let _ = tx_shaders_install.send(LauncherMessage::TaskFailed(
                "Select a valid profile in Profile Editor".to_string(),
            ));
            return;
        };

        sheet_profile_shaders_install.set_open(false);
        let tx = tx_shaders_install.clone();
        thread::spawn(move || {
            let mut installed_count = 0usize;
            for project in selected_projects {
                match install_modrinth_project(DiscoveryKind::Shaders, &project, &profile) {
                    Ok(path) => {
                        installed_count += 1;
                        let _ = tx.send(LauncherMessage::DiscoveryInstallFinished {
                            kind: DiscoveryKind::Shaders,
                            title: project.title,
                            target_path: path.to_string_lossy().to_string(),
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(LauncherMessage::TaskFailed(format!(
                            "Failed to install shader '{}': {}",
                            project.title, error
                        )));
                    }
                }
            }
            let _ = tx.send(LauncherMessage::StatusUpdate(format!(
                "Installed {} shaderpack(s) for '{}'",
                installed_count, profile.name
            )));
            let _ = tx.send(LauncherMessage::DiscoveryInstalledChanged(
                DiscoveryKind::Shaders,
            ));
        });
    });

    // Tracks whether a launch pipeline is currently running.
    let task_active = Rc::new(Cell::new(false));

    // UI-Safe Background Polling Task (Runs every 50ms on the main GUI loop)
    let btn_play_clone = btn_play.clone();
    let btn_switch_clone = btn_switch_user.clone();
    let btn_login_microsoft_clone = btn_login_microsoft.clone();
    let text_view_clone = text_view.clone();
    let lbl_status_clone = lbl_ready_status.clone();
    let lbl_welcome_clone = lbl_welcome_user.clone();
    let row_status_clone = row_account_status.clone();
    let progress_bar_clone = progress_bar.clone();
    let versions_poll = available_versions.clone();
    let profiles_poll = profiles.clone();
    let refresh_profiles_poll = refresh_profile_models.clone();
    let view_stack_poll = view_stack.clone();
    let current_session_poll = current_session.clone();
    let task_active_poll = task_active.clone();
    let render_home_cards_poll = render_home_cards.clone();
    let mods_results_poll = mods_results.clone();
    let shaders_results_poll = shaders_results.clone();
    let lbl_mods_results_status_poll = lbl_profile_mods_results_status.clone();
    let lbl_shaders_results_status_poll = lbl_profile_shaders_results_status.clone();
    let render_mods_cards_poll = render_mods_cards.clone();
    let render_shaders_cards_poll = render_shaders_cards.clone();
    let refresh_mods_installed_poll = refresh_mods_installed_for_selected_profile.clone();
    let refresh_shaders_installed_poll = refresh_shaders_installed_for_selected_profile.clone();

    glib::source::timeout_add_local(Duration::from_millis(50), move || {
        while let Ok(msg) = rx.try_recv() {
            let buffer = text_view_clone.buffer();
            match msg {
                LauncherMessage::VersionsLoaded(versions) => {
                    if versions.is_empty() {
                        lbl_status_clone.set_label("No versions available");
                    } else {
                        *versions_poll.borrow_mut() = versions.clone();

                        if profiles_poll.borrow().is_empty() {
                            profiles_poll
                                .borrow_mut()
                                .push(LauncherProfile::default_with_version(versions[0].clone()));
                            if let Err(e) = save_profiles_to_disk(&profiles_poll.borrow()) {
                                buffer.insert_at_cursor(&format!(
                                    "[WARN] Failed writing default profile to disk: {e}\n"
                                ));
                            }
                        } else {
                            let default_version = versions[0].clone();
                            let mut migrated = false;
                            {
                                let mut profile_list = profiles_poll.borrow_mut();
                                for profile in profile_list.iter_mut() {
                                    if !versions.iter().any(|v| v == &profile.version_id) {
                                        buffer.insert_at_cursor(&format!(
                                            "[WARN] Profile '{}' used unsupported version '{}'; migrated to '{}'\n",
                                            profile.name, profile.version_id, default_version
                                        ));
                                        profile.version_id = default_version.clone();
                                        migrated = true;
                                    }
                                }
                            }

                            if migrated {
                                if let Err(e) = save_profiles_to_disk(&profiles_poll.borrow()) {
                                    buffer.insert_at_cursor(&format!(
                                        "[WARN] Failed saving migrated profiles: {e}\n"
                                    ));
                                }
                            }
                        }

                        (refresh_profiles_poll)(Some("Default".to_string()));
                        lbl_status_clone
                            .set_label(&format!("{} versions available", versions.len()));
                    }
                }
                LauncherMessage::Log(text) => {
                    buffer.insert_at_cursor(&format!("{text}\n"));
                    let mut end = buffer.end_iter();
                    text_view_clone.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                }
                LauncherMessage::StatusUpdate(status) => {
                    lbl_status_clone.set_label(&status);
                }
                LauncherMessage::TaskFinished => {
                    lbl_status_clone.set_label("Game Closed / Task Completed");
                    btn_play_clone.set_sensitive(
                        current_session_poll.borrow().is_some()
                            && !profiles_poll.borrow().is_empty(),
                    );
                    task_active_poll.set(false);
                    progress_bar_clone.set_visible(false);
                    progress_bar_clone.set_fraction(0.0);
                }
                LauncherMessage::TaskFailed(err) => {
                    lbl_status_clone.set_label("Execution Error!");
                    buffer.insert_at_cursor(&format!("[ERROR] {err}\n"));
                    let mut end = buffer.end_iter();
                    text_view_clone.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                    btn_play_clone.set_sensitive(
                        current_session_poll.borrow().is_some()
                            && !profiles_poll.borrow().is_empty(),
                    );
                    task_active_poll.set(false);
                    progress_bar_clone.set_visible(false);
                    progress_bar_clone.set_fraction(0.0);
                }
                LauncherMessage::OpenUrl(url) => {
                    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(
                        &url,
                        None::<&gtk::gio::AppLaunchContext>,
                    ) {
                        buffer.insert_at_cursor(&format!(
                            "[ERROR] Failed to open browser automatically: {e}\nOpen this URL manually:\n{url}\n"
                        ));
                    }
                    let mut end = buffer.end_iter();
                    text_view_clone.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                }
                LauncherMessage::DiscoverySearchResults {
                    kind,
                    query,
                    results,
                } => {
                    let count = results.len();
                    match kind {
                        DiscoveryKind::Mods => {
                            *mods_results_poll.borrow_mut() = results;
                            lbl_mods_results_status_poll
                                .set_label(&format!("{} result(s) for '{}'", count, query));
                            (render_mods_cards_poll)();
                        }
                        DiscoveryKind::Shaders => {
                            *shaders_results_poll.borrow_mut() = results;
                            lbl_shaders_results_status_poll
                                .set_label(&format!("{} result(s) for '{}'", count, query));
                            (render_shaders_cards_poll)();
                        }
                    }
                }
                LauncherMessage::DiscoverySearchFailed { kind, error } => {
                    match kind {
                        DiscoveryKind::Mods => {
                            lbl_mods_results_status_poll
                                .set_label("Modrinth search failed for mods");
                        }
                        DiscoveryKind::Shaders => {
                            lbl_shaders_results_status_poll
                                .set_label("Modrinth search failed for shaders");
                        }
                    }
                    buffer.insert_at_cursor(&format!(
                        "[ERROR] {} search failed: {}\n",
                        kind.label(),
                        error
                    ));
                }
                LauncherMessage::DiscoveryInstallFinished {
                    kind,
                    title,
                    target_path,
                } => {
                    lbl_status_clone.set_label(&format!("{} installed", kind.label()));
                    buffer.insert_at_cursor(&format!("Installed '{}' to {}\n", title, target_path));
                    match kind {
                        DiscoveryKind::Mods => (refresh_mods_installed_poll)(),
                        DiscoveryKind::Shaders => (refresh_shaders_installed_poll)(),
                    }
                    let mut end = buffer.end_iter();
                    text_view_clone.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                }
                LauncherMessage::DiscoveryInstalledChanged(kind) => match kind {
                    DiscoveryKind::Mods => (refresh_mods_installed_poll)(),
                    DiscoveryKind::Shaders => (refresh_shaders_installed_poll)(),
                },
                LauncherMessage::MicrosoftAuthSuccess {
                    username,
                    uuid,
                    access_token,
                    refresh_token,
                } => {
                    *current_session_poll.borrow_mut() = Some(Session::Microsoft {
                        username: username.clone(),
                        uuid,
                        access_token,
                        refresh_token,
                    });
                    lbl_welcome_clone.set_label(&format!("Hello, {username}"));
                    row_status_clone
                        .set_subtitle(&format!("Signed in with Microsoft as {username}"));
                    lbl_status_clone.set_label("Launcher Ready (Online)");
                    btn_play_clone.set_sensitive(!profiles_poll.borrow().is_empty());
                    btn_switch_clone.set_sensitive(true);
                    btn_login_microsoft_clone.set_sensitive(true);
                    view_stack_poll.set_visible_child_name("page_home");
                    (render_home_cards_poll)();
                }
                LauncherMessage::MicrosoftAuthFailed(err) => {
                    lbl_status_clone.set_label("Microsoft sign-in failed");
                    buffer.insert_at_cursor(&format!("[ERROR] {err}\n"));
                    row_status_clone.set_subtitle("Not signed in");
                    btn_login_microsoft_clone.set_sensitive(true);
                    let mut end = buffer.end_iter();
                    text_view_clone.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                }
            }
        }

        if task_active_poll.get() {
            progress_bar_clone.pulse();
        }

        glib::ControlFlow::Continue
    });

    // --- 4. Play Engine Activation ---
    let launch_profile_dropdown = dropdown_profile_launch.clone();
    let current_session_launch = current_session.clone();
    let profiles_launch = profiles.clone();

    let progress_bar_launch = progress_bar.clone();
    let task_active_launch = task_active.clone();

    btn_play.connect_clicked(move |button| {
        let session = match current_session_launch.borrow().clone() {
            Some(session) => session,
            None => return,
        };

        button.set_sensitive(false);

        task_active_launch.set(true);
        progress_bar_launch.set_fraction(0.0);
        progress_bar_launch.set_visible(true);
        progress_bar_launch.pulse();

        let selected_profile_idx = launch_profile_dropdown.selected() as usize;
        let selected_profile = {
            let profile_list = profiles_launch.borrow();
            match profile_list.get(selected_profile_idx) {
                Some(profile) => profile.clone(),
                None => {
                    button.set_sensitive(true);
                    task_active_launch.set(false);
                    progress_bar_launch.set_visible(false);
                    return;
                }
            }
        };
        let target_version = selected_profile.version_id.clone();

        if !available_versions
            .borrow()
            .iter()
            .any(|v| v == &target_version)
        {
            let _ = tx.send(LauncherMessage::TaskFailed(format!(
                "Profile '{}' references unsupported Minecraft version '{}'. Open Profile Editor and choose a valid version.",
                selected_profile.name, target_version
            )));
            button.set_sensitive(true);
            task_active_launch.set(false);
            progress_bar_launch.set_visible(false);
            progress_bar_launch.set_fraction(0.0);
            return;
        }

        let java_path_raw = selected_profile
            .java_binary
            .as_deref()
            .unwrap_or("")
            .to_string();

        let java_install_policy = if selected_profile.java_auto_download {
            mc_launcher_core::install::JavaInstallPolicy::Auto
        } else {
            mc_launcher_core::install::JavaInstallPolicy::Never
        };

        let thread_tx = tx.clone();

        thread::spawn(move || {
            use mc_launcher_core::account::Account;
            use mc_launcher_core::prelude::*;

            let mut effective_session = session;
            if let Session::Microsoft {
                refresh_token,
                username,
                ..
            } = &effective_session
            {
                if let Ok(client_id) = std::env::var(MS_CLIENT_ID_ENV) {
                    let _ = thread_tx.send(LauncherMessage::Log(format!(
                        "Refreshing Microsoft access token for {username}..."
                    )));
                    match complete_microsoft_refresh_resilient(&client_id, refresh_token) {
                        Ok((fresh_username, fresh_uuid, fresh_access_token, fresh_refresh_token)) => {
                            if let Err(e) = save_refresh_token(&fresh_refresh_token) {
                                let _ = thread_tx.send(LauncherMessage::Log(format!(
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
                            let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                "Microsoft token refresh failed before launch: {e}"
                            )));
                            return;
                        }
                    }
                } else {
                    let _ = thread_tx.send(LauncherMessage::Log(
                        "[WARN] LUCENT_MS_CLIENT_ID missing; using existing Microsoft token".to_string(),
                    ));
                }
            }

            thread_tx
                .send(LauncherMessage::StatusUpdate(format!(
                    "Preparing {} ({})",
                    selected_profile.name, target_version
                )))
                .unwrap();
            thread_tx
                .send(LauncherMessage::Log(format!(
                    "Initializing launch pipeline for user: {} (profile: {})",
                    effective_session.display_name(),
                    selected_profile.name
                )))
                .unwrap();

            let mc_dir = match minecraft_root_dir() {
                Ok(path) => path,
                Err(e) => {
                    let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                        "Failed resolving runtime minecraft directory: {e}"
                    )));
                    return;
                }
            };
            let launcher = Launcher::new(mc_dir);

            thread_tx
                .send(LauncherMessage::Log(format!(
                    "Checking manifests and installing profile: MC={}, Loader={}, LoaderVersion={}",
                    selected_profile.version_id,
                    selected_profile.loader_label(),
                    selected_profile.loader_version_label()
                )))
                .unwrap();

            let resolved_forge_version = match (&selected_profile.loader, &selected_profile.loader_version) {
                (ProfileLoader::Forge, ProfileLoaderVersion::LatestStable)
                | (ProfileLoader::Forge, ProfileLoaderVersion::Latest) => {
                    match resolve_latest_forge_version_for_minecraft(&selected_profile.version_id) {
                        Ok(forge_version) => {
                            let _ = thread_tx.send(LauncherMessage::Log(format!(
                                "Resolved Forge version for {} -> {}",
                                selected_profile.version_id, forge_version
                            )));
                            Some(forge_version)
                        }
                        Err(e) => {
                            let _ = thread_tx.send(LauncherMessage::TaskFailed(e));
                            return;
                        }
                    }
                }
                (ProfileLoader::Forge, ProfileLoaderVersion::Exact(v)) => Some(v.clone()),
                _ => None,
            };

            let installed_version_id = if let Some(forge_version) = resolved_forge_version {
                match install_forge_profile_with_java(
                    &launcher,
                    &selected_profile.version_id,
                    &forge_version,
                    &java_path_raw,
                    &thread_tx,
                ) {
                    Ok(version_id) => version_id,
                    Err(e) => {
                        let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                            "Installation pipeline aborted: {}",
                            e
                        )));
                        return;
                    }
                }
            } else {
                let loader_spec = match (&selected_profile.loader, &selected_profile.loader_version) {
                    (ProfileLoader::Vanilla, _) => None,
                    (ProfileLoader::Fabric, ProfileLoaderVersion::LatestStable) => {
                        Some(LoaderSpec::Fabric { version: LoaderVersion::LatestStable })
                    }
                    (ProfileLoader::Fabric, ProfileLoaderVersion::Latest) => {
                        Some(LoaderSpec::Fabric { version: LoaderVersion::Latest })
                    }
                    (ProfileLoader::Fabric, ProfileLoaderVersion::Exact(v)) => {
                        Some(LoaderSpec::Fabric { version: LoaderVersion::Exact(v.clone()) })
                    }
                    (ProfileLoader::Quilt, ProfileLoaderVersion::LatestStable) => {
                        Some(LoaderSpec::Quilt { version: LoaderVersion::LatestStable })
                    }
                    (ProfileLoader::Quilt, ProfileLoaderVersion::Latest) => {
                        Some(LoaderSpec::Quilt { version: LoaderVersion::Latest })
                    }
                    (ProfileLoader::Quilt, ProfileLoaderVersion::Exact(v)) => {
                        Some(LoaderSpec::Quilt { version: LoaderVersion::Exact(v.clone()) })
                    }
                    (ProfileLoader::NeoForge, ProfileLoaderVersion::LatestStable) => {
                        Some(LoaderSpec::NeoForge { version: LoaderVersion::LatestStable })
                    }
                    (ProfileLoader::NeoForge, ProfileLoaderVersion::Latest) => {
                        Some(LoaderSpec::NeoForge { version: LoaderVersion::Latest })
                    }
                    (ProfileLoader::NeoForge, ProfileLoaderVersion::Exact(v)) => {
                        Some(LoaderSpec::NeoForge { version: LoaderVersion::Exact(v.clone()) })
                    }
                    (ProfileLoader::Forge, _) => unreachable!("Forge handled in custom installer path"),
                };

                let install_req = InstallRequest {
                    minecraft_version: selected_profile.version_id.clone(),
                    loader: loader_spec,
                    java: java_install_policy,
                };

                match launcher.install(install_req) {
                    Ok(install_res) => install_res.version_id,
                    Err(e) => {
                        let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                            "Installation pipeline aborted: {:?}",
                            e
                        )));
                        return;
                    }
                }
            };

            thread_tx
                .send(LauncherMessage::Log(format!(
                    "Successfully prepared profile: {}",
                    installed_version_id
                )))
                .unwrap();

            match launcher.load_version(&installed_version_id) {
                Ok(version_meta) => {
                            if let Err(e) = ensure_maven_fallback_libraries_present(
                                &version_meta,
                                launcher.minecraft_dir(),
                                &thread_tx,
                            ) {
                                let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                    "Failed preparing fallback libraries: {e}"
                                )));
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
                                    let _ = thread_tx.send(LauncherMessage::StatusUpdate(
                                        "Repairing mods".to_string(),
                                    ));

                                    match auto_repair_profile_mods(&selected_profile, &thread_tx) {
                                        Ok(summary) => {
                                            let _ = thread_tx.send(LauncherMessage::Log(format!(
                                                "Auto-repair summary: checked={}, updated={}, disabled={}, unknown={}",
                                                summary.checked, summary.updated, summary.disabled, summary.unknown
                                            )));

                                            if !summary.disabled_mods.is_empty() {
                                                let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                                    "Mod repair disabled incompatible mods with no compatible replacements for profile '{}': {}",
                                                    selected_profile.name,
                                                    summary.disabled_mods.join(", ")
                                                )));
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                                "Failed during mod compatibility auto-repair: {e}"
                                            )));
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

                                    let _ = thread_tx.send(LauncherMessage::Log(format!(
                                        "Using profile game directory: {}",
                                        game_dir.display()
                                    )));
                                    let _ = thread_tx.send(LauncherMessage::Log(format!(
                                        "Profile content: {} enabled mod(s), {} enabled shaderpack(s)",
                                        count_enabled(&mods_dir),
                                        count_enabled(&shaders_dir)
                                    )));

                                    options.game_directory = Some(game_dir);
                                }
                                Err(e) => {
                                    let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                        "Failed preparing profile game directory: {e}"
                                    )));
                                    return;
                                }
                            }

                            if !java_path_raw.is_empty() && java_path_raw != "/path/to/binary" {
                                options.java_executable = Some(PathBuf::from(java_path_raw));
                            }

                            match launcher.build_launch_command_from_version(&version_meta, options) {
                                Ok(mut launch_cmd) => {
                                    let injected = apply_profile_runtime_jvm_overrides(
                                        &mut launch_cmd.args,
                                        version_meta.main_class.as_deref(),
                                        selected_profile.java_memory_mb,
                                        selected_profile.java_args.as_deref(),
                                    );
                                    if injected > 0 {
                                        let _ = thread_tx.send(LauncherMessage::Log(format!(
                                            "Applied {injected} profile JVM override argument(s)"
                                        )));
                                    }

                                    let removed = dedupe_launch_classpath(&mut launch_cmd.args);
                                    if removed > 0 {
                                        let _ = thread_tx.send(LauncherMessage::Log(format!(
                                            "Deduplicated {removed} conflicting library entries from classpath"
                                        )));
                                    }

                                    thread_tx
                                        .send(LauncherMessage::StatusUpdate(
                                            "Launching Game Engine".into(),
                                        ))
                                        .unwrap();
                                    thread_tx
                                        .send(LauncherMessage::Log(
                                            "Spawning Java runtime context process...".into(),
                                        ))
                                        .unwrap();

                                    let child_res = std::process::Command::new(&launch_cmd.executable)
                                        .args(&launch_cmd.args)
                                        .current_dir(&launch_cmd.working_dir)
                                        .stdout(Stdio::piped())
                                        .stderr(Stdio::piped())
                                        .spawn();

                                    match child_res {
                                        Ok(mut child) => {
                                            thread_tx
                                                .send(LauncherMessage::StatusUpdate(
                                                    "Minecraft running".into(),
                                                ))
                                                .unwrap();

                                            let stdout_tx = thread_tx.clone();
                                            let stdout_reader = child.stdout.take().map(|stdout| {
                                                thread::spawn(move || {
                                                    let reader = BufReader::new(stdout);
                                                    for line in reader.lines() {
                                                        match line {
                                                            Ok(line) => {
                                                                let _ = stdout_tx.send(LauncherMessage::Log(line));
                                                            }
                                                            Err(e) => {
                                                                let _ = stdout_tx.send(LauncherMessage::Log(format!(
                                                                    "[stdout read error] {e}"
                                                                )));
                                                                break;
                                                            }
                                                        }
                                                    }
                                                })
                                            });

                                            let stderr_tx = thread_tx.clone();
                                            let stderr_reader = child.stderr.take().map(|stderr| {
                                                thread::spawn(move || {
                                                    let reader = BufReader::new(stderr);
                                                    for line in reader.lines() {
                                                        match line {
                                                            Ok(line) => {
                                                                let _ = stderr_tx.send(LauncherMessage::Log(format!(
                                                                    "[stderr] {line}"
                                                                )));
                                                            }
                                                            Err(e) => {
                                                                let _ = stderr_tx.send(LauncherMessage::Log(format!(
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
                                                        let _ = thread_tx.send(LauncherMessage::TaskFinished);
                                                    } else {
                                                        let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                                            "Java process exited with status: {status}"
                                                        )));
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = thread_tx.send(LauncherMessage::TaskFailed(format!(
                                                        "Failed waiting for Java process: {e}"
                                                    )));
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            thread_tx
                                                .send(LauncherMessage::TaskFailed(format!(
                                                    "Failed to spawn Java execution process: {e}"
                                                )))
                                                .unwrap();
                                        }
                                    }
                                }
                                Err(e) => thread_tx
                                    .send(LauncherMessage::TaskFailed(format!(
                                        "Launch command compilation failed: {e:?}"
                                    )))
                                    .unwrap(),
                            }
                        }
                        Err(e) => thread_tx
                            .send(LauncherMessage::TaskFailed(format!(
                                "Failed loading version structural profile: {e:?}"
                            )))
                            .unwrap(),
                    }
        });
    });

    // --- 5. Window Assembly ---
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Lucent Launcher")
        .default_width(1024)
        .default_height(768)
        .content(&toolbar_view)
        .build();

    window.present();
}
