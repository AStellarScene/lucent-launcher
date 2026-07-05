param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$mingwBin = "C:\msys64\mingw64\bin"
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"

if (-not (Test-Path $mingwBin)) {
    throw "Missing $mingwBin. Run scripts/windows/setup-gtk.ps1 first."
}

if (-not (Test-Path (Join-Path $cargoBin "cargo.exe"))) {
    throw "Cargo not found in $cargoBin. Install Rust first."
}

$env:PATH = "$mingwBin;$cargoBin;$env:PATH"
$env:PKG_CONFIG_PATH = "C:/msys64/mingw64/lib/pkgconfig;C:/msys64/mingw64/share/pkgconfig"

if (-not $CargoArgs -or $CargoArgs.Count -eq 0) {
    $CargoArgs = @("check")
}

Push-Location $repoRoot
try {
    & cargo +stable-x86_64-pc-windows-gnu @CargoArgs
} finally {
    Pop-Location
}
