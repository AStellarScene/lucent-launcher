# Lucent Launcher — Agent & Contributor Guide

Guidance for AI agents (and humans) working in this repository. Read this before
making changes so edits stay consistent with the current architecture.

Lucent Launcher is a native Minecraft launcher written in **Rust** with
**GTK 4** and **Libadwaita**. The UI is declarative (`GtkBuilder` XML), while
install/launch logic is provided by a GUI-free, vendored copy of
[`mc-launcher-core`](https://crates.io/crates/mc-launcher-core) under
`crates/mc-launcher-core`.

---

## Tech stack

| Component | Version / feature |
|-----------|-------------------|
| Rust edition | 2024 |
| `libadwaita` (`adw`) | 0.9.1, feature `v1_8` |
| `gtk4` (`gtk`) | 0.11.4, feature `v4_12` |
| vendored `mc-launcher-core` | upstream 0.1.1, local path dependency |
| `keyring` | 3 |
| `serde` / `serde_json` | 1.x |
| `reqwest` | 0.13 (blocking/json/rustls) |
| `url` | 2 |
| `sha1` | 0.10 |

Keep GTK/Adwaita pinned as above; APIs differ across releases.

---

## Project layout

```text
lucent-launcher/
├── .cargo/
│   └── config.toml       # Windows GNU linker + rustflags
├── Cargo.toml            # dependencies & metadata
├── Cargo.lock
├── AGENTS.md             # this file
├── WINDOWS.md            # Windows setup/build/packaging guide
├── README.md             # user-facing architecture/build guide
├── profiles.json         # legacy/runtime profile data (normally under app-data)
├── scripts/
│   └── windows/
│       ├── setup-gtk.ps1     # installs MSYS2 + GTK + Rust GNU toolchain
│       ├── cargo-gnu.ps1     # wrapper for cargo with MSYS2 env vars
│       ├── build-portable.ps1# creates portable app folder/zip (exe + DLLs)
│       ├── build-msix.ps1    # builds and signs MSIX from portable bundle
│       └── msix/
│           └── AppxManifest.xml.template
├── crates/
│   └── mc-launcher-core/ # vendored launcher/install core; app-owned fixes live here
├── src/
│   ├── main.rs           # GTK composition and UI event wiring
│   ├── launch_service.rs # GTK-free launch preparation and process orchestration
│   ├── storage.rs        # app-data migration and versioned profile persistence
│   └── ui/
│       └── launcher.ui   # declarative GtkBuilder UI (embedded at build time)
└── .minecraft/           # runtime game/install data (managed by launcher)
```

Primary application source is `src/main.rs`, `src/launch_service.rs`, `src/storage.rs`, and `src/ui/launcher.ui`. The vendored launcher backend lives in `crates/mc-launcher-core/src/` and is a path dependency so it can be improved with the app. The vendored crate includes its own `LICENSE` and README.

`.minecraft/` and app-data `profiles.json` are runtime data, not source. The repository-root `profiles.json`, when present, is treated as legacy data during migration.

Important runtime subpaths used today:
- `.minecraft/profiles/<profile-id>/` — per-profile game directory; the ID is stable across display-name changes
- `.minecraft/profiles/<profile-id>/mods` — profile-scoped mods
- `.minecraft/profiles/<profile-id>/shaderpacks` — profile-scoped shaderpacks
- files ending in `.disabled` are treated as disabled content

---

## Build & run

```sh
cargo check
cargo build
cargo build --release
cargo run
```

Recommended validation:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --manifest-path crates/mc-launcher-core/Cargo.toml --lib
```

The application requires a display to run. The vendored core tests are GUI-free.

### Windows (GNU + MSYS2)

Use the Windows wrapper scripts instead of plain cargo:

```powershell
./scripts/windows/setup-gtk.ps1
./scripts/windows/cargo-gnu.ps1 build --release
./scripts/windows/build-portable.ps1 -Zip
./scripts/windows/build-msix.ps1 -Version 0.1.0.0
```

Packaging notes:
- Portable output lives in `dist/windows-portable/` and optional zip output in `dist/lucent-launcher-windows-portable.zip`.
- MSIX output lives in `dist/msix/`.
- MSIX requires Windows SDK tooling (`makeappx.exe`, `signtool.exe`).
- A true single-file GTK executable is not expected with MSYS2 GTK; ship exe plus DLLs.

Runtime requirements / gotchas:

- GTK requires a display (headless requires virtual display).
- UI is loaded from embedded GResource (`/com/lucentlauncher/ui/launcher.ui`).
- Runtime root is `.minecraft` under OS app-data path (`LUCENT_DATA_DIR` overrides).
- Active launch game directory is profile-scoped (`<app-data>/.minecraft/profiles/<profile-id>`); IDs remain stable when display names change.
- Profile persistence file is `<app-data>/profiles.json`.
- Legacy data in working directory is migrated when app-data is empty; on migration failure launcher falls back to legacy path for safety.
- Windows GNU target uses static Rust runtime flags in `.cargo/config.toml` (`crt-static` + `-static-libgcc`), while GTK/Libadwaita remain dynamically loaded.

---

## Architecture

### 1) Declarative UI (`src/ui/launcher.ui`)
Widget structure, spacing, and style live in XML. `main.rs` fetches by `id` and
connects behavior.

**Rule:** avoid procedural layout/styling in Rust; add/edit UI in XML.

### 2) Main UI thread (`build_ui`)
GTK widgets are `!Send`/`!Sync`; never move them to worker threads.
Use `Rc<RefCell<_>>` / `Rc<Cell<_>>` for state shared across UI closures.

### 3) Application storage (`src/storage.rs`)
`storage.rs` owns OS app-data resolution, legacy working-directory migration,
versioned profile documents, stable profile IDs, and atomic profile writes. It
uses the vendored core's atomic writer for the same durability guarantees as
Minecraft metadata and downloads.

### 4) Launch service (`src/launch_service.rs`)
`LaunchService` is GTK-free and owns the blocking launch workflow: Microsoft
refresh, Java resolution, loader installation, Forge installation, metadata
loading, fallback-library repair, mod repair, launch-command preparation, Java
process spawning, and process-output streaming. It reports through typed
`AppEvent::Launch` events plus shared log/status events, so the UI remains
responsible only for presentation.

### 5) Background workers (`std::thread`)
Network, install, auth, and game process waits run off the main loop.
Workers communicate via `std::sync::mpsc::channel<AppEvent>`, with domain groups
for versions, authentication, launch, and Modrinth discovery. A
`glib::timeout_add_local` (50 ms) drains messages and updates UI safely.

### 6) Vendored launcher core (`crates/mc-launcher-core`)
The core is a separate path dependency and remains GUI-free. It owns generic
Minecraft installation, metadata, loader, command-building, account-helper,
and progress primitives. Lucent-specific UI, profile policy, Modrinth content
management, keyring storage, and presentation remain in the application.

Core improvements currently maintained in-tree:
- atomic metadata writes;
- verified, transactional downloads;
- structured JVM argument insertion before the main class; and
- classpath deduplication based on Maven artifact identity.

When changing core behavior, add or update a core-level test first.

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
- runtime settings:
  - Java auto-download policy (`Auto`, `Never`)
  - optional Java binary path
  - optional memory limit in MB (`-Xmx` override)
  - optional extra JVM arguments (whitespace-split)

Profile actions:
- create / save / delete
- persisted to a versioned `profiles.json` document
- loaded and migrated at startup
- identified by a stable ID independent of the display name

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
- an available per-profile launch lock

`LaunchService` runs the blocking pipeline on a worker thread:
1. Resolve loader/install strategy from selected profile.
   - Forge uses a custom installer path (captures installer stdout/stderr and ensures `launcher_profiles.json`).
   - Non-Forge loaders use `Launcher::install(request)`.
  - `InstallRequest.java` is profile-driven (`Auto`/`Never`).
2. `Launcher::load_version(version_id)`.
3. Ensure fallback Maven libraries exist for libs that omit `downloads.artifact`.
4. Set `LaunchOptions.game_directory` to profile-scoped directory.
5. Run launch-time mod repair for selected profile:
  - check enabled mod jars against Modrinth hash metadata
  - if incompatible, try compatible update retrieval first
  - only disable as fallback (`.disabled`) when no compatible replacement exists
  - abort launch and report disabled mods if any had to be disabled
6. Build launch command (`build_launch_command_from_version(...)`).
  - apply profile runtime overrides (`java_executable`, memory, extra JVM args)
7. Use the core's `LaunchCommand::deduplicate_classpath()` for conflicting artifacts.
8. Spawn Java process, stream stdout/stderr into Console Log, wait for exit.

Generated launcher metadata, downloaded mods, Java runtime files, Forge
installers, and fallback libraries use the core's atomic writer so interrupted
writes do not appear complete on the next launch. A profile lock prevents the
same profile from being launched by multiple launcher instances, while a shared
Minecraft installation lock serializes preparation of common versions,
libraries, assets, and runtimes. Different profiles can run concurrently after
preparation completes.

### Mods / shaders workflow (Profile Editor)
- Mods and shaders are managed from Profile Editor via Adwaita bottom sheets:
  - `sheet_profile_mods`
  - `sheet_profile_shaders`
- Search uses Modrinth API and renders selectable result cards with checkboxes.
- Search cards intentionally do not render project avatars/icons.
- Install action supports multi-select batch install.
- Selection behavior in Search tab:
  - checked items float into a `Selected for install` section at the top
  - selection persists across repeated searches while sheet is open
- Compatibility is filtered client-side before download:
  - Mods: `game_versions` + loader
  - Shaders: `game_versions`
- Installed items for the selected profile are listed in each sheet and support:
  - enable/disable (via `.disabled` suffix)
  - delete

### Progress/logging
- Installation progress is forwarded from the core through typed launch events.
- Download plans and byte streams use determinate progress when totals are
  available; metadata, Java setup, and external Forge installer work pulse
  indeterminately.
- Console auto-scrolls to newest log lines.
- Minecraft readiness is detected from client initialization output (rendering/audio
  startup markers), then the normal bottom status bar shows `Game Running` until
  all launched profiles exit. Multiple profiles are tracked by stable profile ID
  without changing the bottom bar into a popup or card stack.

---

## Widget ID highlights (current)

### Launch bar
- `dropdown_profile_launch` — profile selector used for launch
- `btn_play` — Launch
- `progress_bar` — phase-aware launch progress bar
- `lbl_progress` — full-width, bottom-aligned launch progress text; keep long
  messages ellipsized so they cannot affect window sizing


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
- `btn_profile_manage_mods`, `btn_profile_manage_shaders`, `btn_profile_open_folder`
- Runtime settings:
  - `dropdown_profile_runtime_java_policy` — Java install policy (`Auto`/`Never`)
  - `row_java_binary` — optional Java executable override
  - `row_profile_runtime_memory_mb` — optional `-Xmx` MB limit
  - `row_profile_runtime_jvm_args` — optional extra JVM args

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
- **Progress totals are phase-scoped:** download plans can report task and
  byte percentages, but dynamically discovered work and external installers
  remain indeterminate.
- **Readiness is log-marker based:** Vanilla and loaders can vary their startup
  output, so readiness is declared when a rendering/audio initialization marker
  is observed rather than through a Minecraft window-management API.
- **UI wiring remains concentrated:** `main.rs` is still large; the next split should extract page controllers while keeping `LaunchService` and the vendored core UI-independent.
- **JVM args parsing is basic:** extra JVM arguments are split on whitespace;
  quoting/escaped spaces are not currently supported.
- The vendored core is based on `mc-launcher-core` 0.1.1 and is intentionally kept GUI-free. Local backend changes should be covered by core-level tests before changing the app-facing facade.
