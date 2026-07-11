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

Windows GNU builds now include static Rust runtime flags via `.cargo/config.toml`:
- `-C target-feature=+crt-static`
- `-C link-arg=-static-libgcc`

## 3) Run

```powershell
./scripts/windows/cargo-gnu.ps1 run
```

## 4) Portable (self-contained folder)

To produce a self-contained Windows app folder (exe + required GTK/MSYS2 DLLs):

```powershell
./scripts/windows/build-portable.ps1 -Zip
```

Artifacts:
- `dist/windows-portable/`
- `dist/lucent-launcher-windows-portable.zip`

## 5) MSIX installer

Create an MSIX package from the portable bundle:

```powershell
./scripts/windows/build-msix.ps1 -Version 0.1.0.0
```

This script:
- builds portable files (unless `-SkipPortableBuild`)
- generates `AppxManifest.xml` from template
- creates package at `dist/msix/<PackageName>_<Version>_x64.msix`
- signs package (default: auto-generated dev cert)

Useful options:

```powershell
# Use your own signing certificate
./scripts/windows/build-msix.ps1 -Version 0.1.0.0 -CertificatePfxPath C:\certs\code-signing.pfx -CertificatePassword (ConvertTo-SecureString "<password>" -AsPlainText -Force)

# Build unsigned (for CI handoff to separate signing step)
./scripts/windows/build-msix.ps1 -Version 0.1.0.0 -Unsigned

# Reuse existing portable output
./scripts/windows/build-msix.ps1 -Version 0.1.0.0 -SkipPortableBuild
```

## Notes

- `.cargo/config.toml` pins the Windows GNU linker/ar paths.
- `scripts/windows/cargo-gnu.ps1` sets `PKG_CONFIG` and `PKG_CONFIG_PATH` for MSYS2.
- A true single-file GTK executable is not supported with the MSYS2 GTK stack; use the portable folder/zip artifact.
- MSIX tooling requires Windows SDK (`makeappx.exe`, `signtool.exe`).
- The UI is embedded into the executable as a GResource; runtime does not require the repository's `src/ui/launcher.ui` path.
- If `winget` is unavailable, install MSYS2 manually, then run:
  - `pacman -Syu --noconfirm`
  - `pacman -S --noconfirm --needed mingw-w64-x86_64-toolchain mingw-w64-x86_64-pkgconf mingw-w64-x86_64-gtk4 mingw-w64-x86_64-libadwaita`
