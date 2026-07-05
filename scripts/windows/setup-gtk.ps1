$ErrorActionPreference = "Stop"

$msysRoot = "C:\msys64"
$bash = Join-Path $msysRoot "usr\bin\bash.exe"

Write-Host "Installing MSYS2 (if missing)..."
if (-not (Test-Path $bash)) {
    winget install --id MSYS2.MSYS2 -e --accept-source-agreements --accept-package-agreements
}

if (-not (Test-Path $bash)) {
    throw "MSYS2 installation not found at $bash after winget install."
}

Write-Host "Updating MSYS2 packages..."
& $bash -lc "pacman -Syu --noconfirm"

Write-Host "Installing GTK4/Libadwaita toolchain packages..."
& $bash -lc "pacman -S --noconfirm --needed mingw-w64-x86_64-toolchain mingw-w64-x86_64-pkgconf mingw-w64-x86_64-gtk4 mingw-w64-x86_64-libadwaita"

Write-Host "Installing Rust GNU toolchain..."
rustup toolchain install stable-x86_64-pc-windows-gnu

Write-Host "Setup complete. Use scripts/windows/cargo-gnu.ps1 to build or run."
