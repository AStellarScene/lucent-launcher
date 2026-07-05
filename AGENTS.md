# Lucent Launcher — Agent & Contributor Guide

Guidance for AI agents (and humans) working in this repository. Read this before
making changes so edits stay consistent with the current architecture.

Lucent Launcher is a native Minecraft launcher written in **Rust** with
**GTK 4** and **Libadwaita**. The UI is declarative (`GtkBuilder` XML), while
install/launch logic is delegated to
[`mc-launcher-core`](https://crates.io/crates/mc-launcher-core).

---

## Tech stack

| Component | Version / feature |
|-----------|-------------------|
| Rust edition | 2024 |
| `libadwaita` (`adw`) | 0.9.1, feature `v1_8` |
| `gtk4` (`gtk`) | 0.11.4, feature `v4_12` |
| `mc-launcher-core` | 0.1 |
| `keyring` | 3 |
| `serde` / `serde_json` | 1.x |
| `reqwest` | 0.13 (blocking/json/rustls) |
| `url` | 2 |

Keep GTK/Adwaita pinned as above; APIs differ across releases.

---

## Project layout

```text
lucent-launcher/
├── Cargo.toml            # dependencies & metadata
├── Cargo.lock
├── AGENTS.md             # this file
├── profiles.json         # persisted launcher profiles (created at runtime)
├── src/
│   ├── main.rs           # app logic (single-file for now)
│   └── ui/
│       └── launcher.ui   # declarative GtkBuilder UI (loaded at runtime)
└── .minecraft/           # runtime game/install data (managed by launcher)
```

Primary source remains `src/main.rs` + `src/ui/launcher.ui`.
`.minecraft/` and `profiles.json` are runtime data, not source.

Important runtime subpaths used today:
- `.minecraft/profiles/<profile-name>/` — per-profile game directory
- `.minecraft/profiles/<profile-name>/mods` — profile-scoped mods
- `.minecraft/profiles/<profile-name>/shaderpacks` — profile-scoped shaderpacks
- files ending in `.disabled` are treated as disabled content

---

## Build & run

```sh
cargo build
cargo build --release
cargo run
```

Runtime requirements / gotchas:

- GTK requires a display (headless requires virtual display).
- `launcher.ui` is loaded from relative path (`src/ui/launcher.ui`) so run from project root.
- Runtime root is `.minecraft` under current working directory.
- Active launch game directory is profile-scoped (`.minecraft/profiles/<profile-name>`).
- Profile persistence file is `profiles.json` in current working directory.

---

## Architecture

### 1) Declarative UI (`src/ui/launcher.ui`)
Widget structure, spacing, and style live in XML. `main.rs` fetches by `id` and
connects behavior.

**Rule:** avoid procedural layout/styling in Rust; add/edit UI in XML.

### 2) Main UI thread (`build_ui`)
GTK widgets are `!Send`/`!Sync`; never move them to worker threads.
Use `Rc<RefCell<_>>` / `Rc<Cell<_>>` for state shared across UI closures.

### 3) Background workers (`std::thread`)
Network, install, auth, and game process waits run off the main loop.
Workers communicate via `std::sync::mpsc::channel<LauncherMessage>`.
A `glib::timeout_add_local` (50 ms) drains messages and updates UI safely.

---

## Current application flow

### Versions
- On startup, a worker fetches release versions from `mc_launcher_core::utils::get_version_list()`.
- Version list drives profile-version selectors.

### Profiles (Profile Editor page)
Profiles are first-class launch targets.
Each profile stores:
- name
- Minecraft version
- loader kind (`Vanilla`, `Fabric`, `Quilt`, `Forge`, `NeoForge`)
- loader version mode (`LatestStable`, `Latest`, `Exact(...)`)
- card color mode (`Auto`, `Custom`) + optional color hex

Profile actions:
- create / save / delete
- persisted to `profiles.json`
- loaded at startup

### Account / auth
- Offline login is supported.
- Microsoft OAuth flow is wired with loopback redirect + PKCE.
- Refresh token is stored in OS keyring.

**Important:** successful OAuth does not guarantee Xbox/Minecraft service access.
If `login_with_xbox` returns `Invalid app registration`, app registration approval is missing.

### Launch
Launch requires:
- active session
- selected profile

Launch worker pipeline:
1. Resolve loader/install strategy from selected profile.
   - Forge uses a custom installer path (captures installer stdout/stderr and ensures `launcher_profiles.json`).
   - Non-Forge loaders use `Launcher::install(request)`.
2. `Launcher::load_version(version_id)`.
3. Ensure fallback Maven libraries exist for libs that omit `downloads.artifact`.
4. Build launch command (`build_launch_command_from_version(...)`).
5. Set `LaunchOptions.game_directory` to profile-scoped directory.
6. Dedupe conflicting classpath entries where needed.
7. Spawn Java process, stream stdout/stderr into Console Log, wait for exit.

### Mods / shaders workflow (Profile Editor)
- Mods and shaders are managed from Profile Editor via Adwaita bottom sheets:
  - `sheet_profile_mods`
  - `sheet_profile_shaders`
- Search uses Modrinth API and renders selectable result cards with checkboxes.
- Install action supports multi-select batch install.
- Compatibility is filtered client-side before download:
  - Mods: `game_versions` + loader
  - Shaders: `game_versions`
- Installed items for the selected profile are listed in each sheet and support:
  - enable/disable (via `.disabled` suffix)
  - delete

### Progress/logging
- Progress bar is indeterminate pulse while task active.
- Console auto-scrolls to newest log lines.

---

## Widget ID highlights (current)

### Launch bar
- `dropdown_profile_launch` — profile selector used for launch
- `btn_play` — Launch
- `progress_bar` — indeterminate loading bar

### Account
- `row_login_username`, `btn_login`, `btn_login_microsoft`
- `row_account_status`, `btn_switch_user`

### Profile Editor
- `dropdown_profile_editor` — select profile to edit
- `row_profile_name` — profile name
- `dropdown_profile_version` — Minecraft version
- `dropdown_profile_loader` — loader type
- `dropdown_profile_loader_version_mode` — loader version strategy
- `row_profile_loader_version_exact` — exact loader version input
- `btn_profile_create`, `btn_profile_save`, `btn_profile_delete`
- `btn_profile_manage_mods`, `btn_profile_manage_shaders`
- `row_java_binary` — optional Java executable override

### Profile Editor bottom sheets
- Mods sheet:
  - `sheet_profile_mods`
  - search: `entry_profile_mods_search`, `btn_profile_mods_search`
  - results: `flow_profile_mods_results`
  - installed list: `flow_profile_mods_installed`
  - actions: `btn_profile_mods_sheet_install`, `btn_profile_mods_sheet_cancel`
- Shaders sheet:
  - `sheet_profile_shaders`
  - search: `entry_profile_shaders_search`, `btn_profile_shaders_search`
  - results: `flow_profile_shaders_results`
  - installed list: `flow_profile_shaders_installed`
  - actions: `btn_profile_shaders_sheet_install`, `btn_profile_shaders_sheet_cancel`

### Status/log
- `text_view` — console
- `lbl_welcome_user`, `lbl_ready_status`

Stack pages: `page_account`, `page_home`, `page_console`, `page_profile`.

---

## Conventions & guardrails

1. UI layout/style in XML; Rust only wiring/logic.
2. No GTK widget access from worker threads.
3. Keep blocking work off main loop.
4. Prefer `mc-launcher-core` for install/launch behavior.
5. Keep edits focused; preserve existing IDs when possible.
6. Run `cargo build` after changes.

---

## Known limitations / future work

- **App registration gating for Microsoft/Xbox/Minecraft auth:**
  OAuth is wired, but `api.minecraftservices.com/authentication/login_with_xbox`
  may reject unapproved app registrations.
- **No real percentage progress yet:** progress bar is still indeterminate.
- **Single-file app logic:** `main.rs` is large; candidate for modular split
  (auth, profiles, launch, messaging, persistence).
- **UI path is still relative:** for portability, bundle UI with `gio::Resource`.
