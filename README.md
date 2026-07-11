# Lucent Launcher

Lucent Launcher is a native Minecraft launcher built with Rust, GTK 4, and
Libadwaita. The interface is declared in GtkBuilder XML, while installation,
metadata handling, launch-command construction, and process preparation run in
Rust worker code.

## Current architecture

Lucent is a modular monolith with two important boundaries:

```text
GTK UI (src/main.rs, src/ui/launcher.ui)
        |
        | mpsc::AppEvent (typed event groups) + GTK main-loop polling
        v
Application storage and orchestration (src/storage.rs, main.rs)
        |
        v
Vendored launcher core (crates/mc-launcher-core)
```

### Application layer

- `src/main.rs` composes the GTK window, connects widget signals, manages UI
  state, and starts background workers.
- `src/launch_service.rs` owns the GTK-free blocking launch pipeline and sends
  progress, logs, completion, and failure events back to the application.
- `src/messages.rs` defines the worker-to-UI `AppEvent` envelope. Events are
  grouped by responsibility into `VersionsEvent`, `AuthEvent`, `LaunchEvent`,
  and `DiscoveryEvent`, keeping the GTK dispatcher explicit and exhaustively
  matched.
- `src/ui/launcher.ui` contains the declarative layout and styling.
- `src/storage.rs` owns app-data resolution, legacy migration, profile schema
  handling, stable profile IDs, and atomic profile persistence.
- Modrinth discovery, profile-scoped content management, Microsoft keyring
  integration, launch coordination/locking, and user-facing status/log messages
  remain application concerns.

GTK widgets are only accessed on the main thread. Blocking network, install,
authentication, and game-process work runs on worker threads. Workers communicate
through `std::sync::mpsc::Sender<AppEvent>`; the GTK main loop is the only place
that touches widgets and drains those messages.

### Vendored launcher core

`crates/mc-launcher-core` is the in-repository copy of the upstream `0.1.1`
launcher library. It is a separate Rust library crate, not flattened into the
GTK application. The path dependency is declared in `Cargo.toml` so Lucent can
fix backend behavior and test those fixes without waiting for a registry
release.

The core remains GUI-free and provides:

- Vanilla, Fabric, Quilt, Forge, and NeoForge installation primitives
- Version metadata loading and inheritance merging
- Library, asset, native, and loader installation
- Structured Java launch-command construction
- Account and authentication helpers
- Platform compatibility handling
- Progress reporting and typed launcher errors

Lucent-owned core improvements currently include:

- Atomic writes for metadata and generated launcher files
- Transactional downloads that validate payloads before replacing destinations
- `LaunchCommand::insert_jvm_arguments()` for safe JVM/game argument placement
- `LaunchCommand::deduplicate_classpath()` for conflicting Maven artifacts

Keep this crate independent of GTK. Add core-level tests for backend changes
before changing application-facing behavior.

## Profiles and runtime data

Profiles are persisted under the OS app-data directory. `LUCENT_DATA_DIR` can be
used to override that location for development or portable setups.

The profile document is versioned and currently has this shape:

```json
{
  "schema_version": 1,
  "profiles": [
    {
      "id": "profile-...",
      "name": "Default",
      "version_id": "1.21.4",
      "loader": "Vanilla",
      "loader_version": "LatestStable"
    }
  ]
}
```

The full profile also stores card color and runtime Java settings. Profile IDs
are stable storage identities; display names can change without moving the
profile's game data. A per-profile OS lock prevents duplicate launches, while a
shared installation lock protects common Minecraft files during preparation.

Runtime layout:

```text
<app-data>/
├── profiles.json
└── .minecraft/
    ├── versions/
    ├── libraries/
    ├── assets/
    └── profiles/
        └── <profile-id>/
            ├── mods/
            └── shaderpacks/
```

On first startup after the storage migration:

- Legacy array-form profile JSON is converted to the versioned document.
- Missing profile IDs are generated and persisted.
- Existing name-based profile directories are moved to their stable-ID paths
  when the new path does not already exist.
- Legacy working-directory data is migrated to the OS app-data directory when
  possible. Migration failures fall back to the legacy path for safety.

Files ending in `.disabled` are treated as disabled mods or shaderpacks.

## Build and run

Linux/macOS development requires GTK 4, Libadwaita, and their development
metadata to be available through `pkg-config`.

```sh
cargo check
cargo build
cargo run
```

Recommended validation before submitting changes:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --manifest-path crates/mc-launcher-core/Cargo.toml --lib
```

The GTK application requires a graphical display. Core tests do not require a
Minecraft installation or a GTK display.

When a Minecraft client reaches a rendering/audio initialization marker, the
normal bottom status bar changes to `Game Running`. Multiple profiles can run at
the same time; the launcher keeps the status active until every launched
profile exits, without opening a separate popup or card stack.

## Windows

Windows builds use the GNU Rust toolchain and MSYS2 GTK libraries. See
[`WINDOWS.md`](WINDOWS.md) for setup, portable packaging, and MSIX commands.

```powershell
./scripts/windows/setup-gtk.ps1
./scripts/windows/cargo-gnu.ps1 check
./scripts/windows/cargo-gnu.ps1 build --release
./scripts/windows/build-portable.ps1 -Zip
```

A single-file GTK executable is not expected; ship the executable together with
its GTK/Libadwaita DLLs.

## Known limitations

- Microsoft/Xbox/Minecraft service access can still require approved app
  registration even when OAuth succeeds.
- Installation progress is determinate for download plans and individual files
  when the server provides sizes; metadata resolution, Java setup, and external
  Forge installers remain indeterminate because they do not expose reliable
  totals.
- Extra JVM arguments are split on whitespace and do not support shell-style
  quoting or escaped spaces.
- Game readiness is inferred from rendering/audio initialization log markers;
  loaders with unusual startup output may remain in the launching state until
  the process exits.
- `src/main.rs` still contains substantial page wiring; future work can extract
  page controllers while preserving the `LaunchService` and vendored core
  boundaries.
