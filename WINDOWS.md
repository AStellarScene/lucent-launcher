# Windows Build And Run

This project now builds on Windows using the GNU Rust toolchain and MSYS2 GTK libraries.

## 1) One-time setup

From PowerShell in the repository root:

```powershell
./scripts/windows/setup-gtk.ps1
```

This installs:
- MSYS2
- `pkg-config`
- GTK4
- Libadwaita
- Rust toolchain `stable-x86_64-pc-windows-gnu`

## 2) Build/check/lint

Use the wrapper script (it sets required environment variables):

```powershell
./scripts/windows/cargo-gnu.ps1 check
./scripts/windows/cargo-gnu.ps1 build
./scripts/windows/cargo-gnu.ps1 fmt --all --check
./scripts/windows/cargo-gnu.ps1 clippy --all-targets
```

## 3) Run

```powershell
./scripts/windows/cargo-gnu.ps1 run
```

## Notes

- `.cargo/config.toml` pins the Windows GNU linker/ar paths.
- `scripts/windows/cargo-gnu.ps1` sets `PKG_CONFIG` and `PKG_CONFIG_PATH` for MSYS2.
- Runtime still expects `src/ui/launcher.ui` relative to repository root.
- If `winget` is unavailable, install MSYS2 manually, then run:
  - `pacman -Syu --noconfirm`
  - `pacman -S --noconfirm --needed mingw-w64-x86_64-toolchain mingw-w64-x86_64-pkgconf mingw-w64-x86_64-gtk4 mingw-w64-x86_64-libadwaita`
