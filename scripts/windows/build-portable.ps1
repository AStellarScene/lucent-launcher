param(
    [switch]$SkipBuild,
    [switch]$Zip
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
$env:PKG_CONFIG = "C:/msys64/mingw64/bin/pkg-config.exe"
$env:PKG_CONFIG_PATH = "C:/msys64/mingw64/lib/pkgconfig;C:/msys64/mingw64/share/pkgconfig"
$env:PKG_CONFIG_ALLOW_SYSTEM_CFLAGS = "1"

$objdump = Join-Path $mingwBin "objdump.exe"
if (-not (Test-Path $objdump)) {
    throw "Missing $objdump. Ensure mingw-w64-x86_64-toolchain is installed."
}

function Get-ImportedDllNames {
    param([Parameter(Mandatory = $true)][string]$BinaryPath)

    if (-not (Test-Path $BinaryPath)) {
        return @()
    }

    $imports = & $objdump -p $BinaryPath | Select-String "DLL Name:"
    $names = @()
    foreach ($line in $imports) {
        $name = ($line.ToString() -replace ".*DLL Name:\s*", "").Trim()
        if ($name) {
            $names += $name
        }
    }
    return $names | Sort-Object -Unique
}

function Resolve-MingwDependencySet {
    param(
        [Parameter(Mandatory = $true)][string]$ExePath,
        [Parameter(Mandatory = $true)][string]$MingwBinPath
    )

    $systemDlls = @(
        "KERNEL32.dll", "USER32.dll", "ADVAPI32.dll", "SHELL32.dll", "OLE32.dll", "OLEAUT32.dll",
        "GDI32.dll", "GDI32full.dll", "IMM32.dll", "WS2_32.dll", "COMDLG32.dll", "COMCTL32.dll",
        "SETUPAPI.dll", "CFGMGR32.dll", "CRYPT32.dll", "WINMM.dll", "VERSION.dll", "MSVCRT.dll",
        "NTDLL.dll", "RPCRT4.dll", "UCRTBASE.dll", "SHLWAPI.dll", "NORMALIZ.dll", "bcrypt.dll",
        "dwmapi.dll", "dxgi.dll", "opengl32.dll", "secur32.dll", "IPHLPAPI.DLL", "WINHTTP.dll"
    )

    $ignore = @{}
    foreach ($dll in $systemDlls) {
        $ignore[$dll.ToLowerInvariant()] = $true
    }

    $seen = @{}
    $resolved = @{}
    $queue = [System.Collections.Generic.Queue[string]]::new()

    foreach ($dllName in (Get-ImportedDllNames -BinaryPath $ExePath)) {
        $queue.Enqueue($dllName)
    }

    while ($queue.Count -gt 0) {
        $dllName = $queue.Dequeue()
        $key = $dllName.ToLowerInvariant()

        if ($seen.ContainsKey($key)) {
            continue
        }
        $seen[$key] = $true

        if ($ignore.ContainsKey($key)) {
            continue
        }

        $candidate = Join-Path $MingwBinPath $dllName
        if (-not (Test-Path $candidate)) {
            Write-Warning "Could not find dependency in MinGW bin: $dllName"
            continue
        }

        $resolved[$key] = $candidate

        foreach ($child in (Get-ImportedDllNames -BinaryPath $candidate)) {
            $queue.Enqueue($child)
        }
    }

    return $resolved.Values | Sort-Object -Unique
}

Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        & cargo +stable-x86_64-pc-windows-gnu build --release
    }

    $exeName = "lucent-launcher.exe"
    $exeSource = Join-Path $repoRoot "target\release\$exeName"
    if (-not (Test-Path $exeSource)) {
        throw "Release executable not found at $exeSource"
    }

    $distRoot = Join-Path $repoRoot "dist"
    $portableDir = Join-Path $distRoot "windows-portable"

    if (Test-Path $portableDir) {
        Remove-Item -Recurse -Force $portableDir
    }

    New-Item -ItemType Directory -Path $portableDir | Out-Null
    Copy-Item $exeSource -Destination (Join-Path $portableDir $exeName)

    $deps = Resolve-MingwDependencySet -ExePath $exeSource -MingwBinPath $mingwBin
    foreach ($dep in $deps) {
        Copy-Item $dep -Destination (Join-Path $portableDir ([System.IO.Path]::GetFileName($dep)))
    }

    if ($Zip) {
        $zipPath = Join-Path $distRoot "lucent-launcher-windows-portable.zip"
        if (Test-Path $zipPath) {
            Remove-Item -Force $zipPath
        }
        Compress-Archive -Path (Join-Path $portableDir "*") -DestinationPath $zipPath
        Write-Host "Portable zip created: $zipPath"
    }

    Write-Host "Portable app folder created: $portableDir"
    Write-Host "Files in bundle:"
    Get-ChildItem $portableDir | Select-Object Name
} finally {
    Pop-Location
}