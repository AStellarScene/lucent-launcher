param(
    [string]$Version = "0.1.0.0",
    [string]$PackageName = "LucentLauncher",
    [string]$DisplayName = "Lucent Launcher",
    [string]$Publisher = "CN=LucentLauncherDev",
    [string]$PublisherDisplayName = "Lucent Launcher",
    [switch]$SkipPortableBuild,
    [string]$CertificatePfxPath,
    [SecureString]$CertificatePassword,
    [switch]$Unsigned
)

$ErrorActionPreference = "Stop"

function Get-SdkToolPath {
    param(
        [Parameter(Mandatory = $true)][string]$ToolName
    )

    $sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    if (-not (Test-Path $sdkRoot)) {
        throw "Windows SDK not found at $sdkRoot. Install Windows 10/11 SDK."
    }

    $candidates = Get-ChildItem -Path $sdkRoot -Directory |
        Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Name } -Descending

    foreach ($candidate in $candidates) {
        $path = Join-Path $candidate.FullName "x64\$ToolName"
        if (Test-Path $path) {
            return $path
        }
    }

    throw "$ToolName not found under $sdkRoot"
}

function New-PlaceholderPng {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][int]$Width,
        [Parameter(Mandatory = $true)][int]$Height,
        [Parameter(Mandatory = $true)][string]$Text
    )

    Add-Type -AssemblyName System.Drawing

    $bitmap = New-Object System.Drawing.Bitmap($Width, $Height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.Clear([System.Drawing.Color]::FromArgb(255, 20, 24, 36))

    $fontSize = [Math]::Max(8, [Math]::Floor([Math]::Min($Width, $Height) / 4))
    $font = New-Object System.Drawing.Font("Segoe UI", $fontSize, [System.Drawing.FontStyle]::Bold)
    $brush = [System.Drawing.Brushes]::White
    $format = New-Object System.Drawing.StringFormat
    $format.Alignment = [System.Drawing.StringAlignment]::Center
    $format.LineAlignment = [System.Drawing.StringAlignment]::Center

    $rect = New-Object System.Drawing.RectangleF(0, 0, $Width, $Height)
    $graphics.DrawString($Text, $font, $brush, $rect, $format)

    $dir = Split-Path -Parent $Path
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Path $dir | Out-Null
    }

    $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)

    $graphics.Dispose()
    $font.Dispose()
    $bitmap.Dispose()
}

if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') {
    throw "Version must be in a.b.c.d format for MSIX (example: 1.2.3.0)."
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$portableScript = Join-Path $repoRoot "scripts\windows\build-portable.ps1"

Push-Location $repoRoot
try {
    if (-not $SkipPortableBuild) {
        & $portableScript
    }

    $portableDir = Join-Path $repoRoot "dist\windows-portable"
    if (-not (Test-Path $portableDir)) {
        throw "Portable folder missing at $portableDir. Run scripts/windows/build-portable.ps1 first."
    }

    $msixRoot = Join-Path $repoRoot "dist\msix"
    $packageDir = Join-Path $msixRoot "package"

    if (Test-Path $packageDir) {
        Remove-Item -Recurse -Force $packageDir
    }

    New-Item -ItemType Directory -Path $packageDir | Out-Null
    Copy-Item -Path (Join-Path $portableDir "*") -Destination $packageDir -Recurse -Force

    $assetsDir = Join-Path $packageDir "Assets"
    New-Item -ItemType Directory -Path $assetsDir -Force | Out-Null

    $square44 = Join-Path $assetsDir "Square44x44Logo.png"
    $square150 = Join-Path $assetsDir "Square150x150Logo.png"
    $wide310 = Join-Path $assetsDir "Wide310x150Logo.png"
    $storeLogo = Join-Path $assetsDir "StoreLogo.png"

    if (-not (Test-Path $square44)) { New-PlaceholderPng -Path $square44 -Width 44 -Height 44 -Text "LL" }
    if (-not (Test-Path $square150)) { New-PlaceholderPng -Path $square150 -Width 150 -Height 150 -Text "LL" }
    if (-not (Test-Path $wide310)) { New-PlaceholderPng -Path $wide310 -Width 310 -Height 150 -Text "LUCENT" }
    if (-not (Test-Path $storeLogo)) { New-PlaceholderPng -Path $storeLogo -Width 50 -Height 50 -Text "LL" }

    $manifestTemplate = Join-Path $repoRoot "scripts\windows\msix\AppxManifest.xml.template"
    if (-not (Test-Path $manifestTemplate)) {
        throw "Manifest template missing at $manifestTemplate"
    }

    $manifestContent = Get-Content -Raw $manifestTemplate
    $manifestContent = $manifestContent.Replace("__PACKAGE_NAME__", $PackageName)
    $manifestContent = $manifestContent.Replace("__PUBLISHER__", $Publisher)
    $manifestContent = $manifestContent.Replace("__PACKAGE_VERSION__", $Version)
    $manifestContent = $manifestContent.Replace("__DISPLAY_NAME__", $DisplayName)
    $manifestContent = $manifestContent.Replace("__PUBLISHER_DISPLAY_NAME__", $PublisherDisplayName)

    $manifestPath = Join-Path $packageDir "AppxManifest.xml"
    Set-Content -Path $manifestPath -Value $manifestContent -NoNewline -Encoding UTF8

    $makeAppx = Get-SdkToolPath -ToolName "makeappx.exe"
    $signTool = Get-SdkToolPath -ToolName "signtool.exe"

    $packageFileName = "$PackageName`_$Version`_x64.msix"
    $msixPath = Join-Path $msixRoot $packageFileName

    if (Test-Path $msixPath) {
        Remove-Item -Force $msixPath
    }

    & $makeAppx pack /d $packageDir /p $msixPath /o

    if ($LASTEXITCODE -ne 0) {
        throw "makeappx failed with exit code $LASTEXITCODE"
    }

    if (-not $Unsigned) {
        $effectivePfxPath = $CertificatePfxPath
        $effectivePasswordPlain = if ($CertificatePassword) {
            [System.Net.NetworkCredential]::new("", $CertificatePassword).Password
        } else {
            ""
        }

        if (-not $effectivePfxPath) {
            $devPfx = Join-Path $msixRoot "dev-signing.pfx"
            $devCer = Join-Path $msixRoot "dev-signing.cer"

            $securePassword = if ($CertificatePassword) {
                $CertificatePassword
            } else {
                ConvertTo-SecureString -String "lucent-dev" -AsPlainText -Force
            }
            if (-not $effectivePasswordPlain) {
                $effectivePasswordPlain = "lucent-dev"
            }

            $existing = Get-ChildItem Cert:\CurrentUser\My |
                Where-Object { $_.Subject -eq $Publisher } |
                Sort-Object NotAfter -Descending |
                Select-Object -First 1

            if (-not $existing) {
                $existing = New-SelfSignedCertificate -Type CodeSigningCert -Subject $Publisher -CertStoreLocation Cert:\CurrentUser\My
            }

            Export-PfxCertificate -Cert $existing -FilePath $devPfx -Password $securePassword | Out-Null
            Export-Certificate -Cert $existing -FilePath $devCer | Out-Null

            $effectivePfxPath = $devPfx

            Write-Host "Created dev signing cert and exported to:"
            Write-Host "  $devPfx"
            Write-Host "  $devCer"
        }

        if (-not (Test-Path $effectivePfxPath)) {
            throw "PFX not found at $effectivePfxPath"
        }

        $signArgs = @("sign", "/fd", "SHA256", "/f", $effectivePfxPath)
        if ($effectivePasswordPlain) {
            $signArgs += @("/p", $effectivePasswordPlain)
        }
        $signArgs += $msixPath

        & $signTool @signArgs

        if ($LASTEXITCODE -ne 0) {
            throw "signtool failed with exit code $LASTEXITCODE"
        }
    }

    Write-Host "MSIX created: $msixPath"
    if ($Unsigned) {
        Write-Host "Package is unsigned; sign before installing on Windows."
    } else {
        Write-Host "Package is signed."
    }
    Write-Host "Package folder used: $packageDir"
} finally {
    Pop-Location
}
