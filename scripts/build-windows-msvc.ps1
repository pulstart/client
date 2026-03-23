param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release",
    [string]$Triplet = "x64-windows",
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$VcpkgRoot = $env:VCPKG_ROOT,
    [switch]$BootstrapVcpkg
)

$ErrorActionPreference = "Stop"
$clientRoot = Split-Path -Parent $PSScriptRoot

if (-not $VcpkgRoot) {
    throw "Set VCPKG_ROOT or pass -VcpkgRoot."
}

$vcpkgExe = Join-Path $VcpkgRoot "vcpkg.exe"
if (-not (Test-Path $vcpkgExe)) {
    if (-not $BootstrapVcpkg) {
        throw "vcpkg.exe not found at '$vcpkgExe'. Re-run with -BootstrapVcpkg after cloning vcpkg."
    }

    $bootstrapScript = Join-Path $VcpkgRoot "bootstrap-vcpkg.bat"
    if (-not (Test-Path $bootstrapScript)) {
        throw "bootstrap-vcpkg.bat not found at '$bootstrapScript'."
    }

    & $bootstrapScript
    if ($LASTEXITCODE -ne 0) {
        throw "vcpkg bootstrap failed."
    }
}

$installRoot = Join-Path $clientRoot "vcpkg_installed"
$installedRoot = Join-Path $installRoot $Triplet
$opusRoot = if ($Configuration -eq "release") {
    $installedRoot
} else {
    Join-Path $installedRoot "debug"
}
$dllDir = if ($Configuration -eq "release") {
    Join-Path $installedRoot "bin"
} else {
    Join-Path $installedRoot "debug\bin"
}
$profile = if ($Configuration -eq "release") { "release" } else { "debug" }

Push-Location $clientRoot
try {
    $env:VCPKG_ROOT = $VcpkgRoot
    $env:VCPKGRS_DYNAMIC = "1"
    $env:VCPKGRS_TRIPLET = $Triplet
    $env:VCPKG_DEFAULT_TRIPLET = $Triplet
    if ($Configuration -eq "release") {
        $env:VCPKG_BUILD_TYPE = "release"
    } else {
        Remove-Item Env:VCPKG_BUILD_TYPE -ErrorAction SilentlyContinue
    }

    & $vcpkgExe install --triplet $Triplet --x-manifest-root=$clientRoot --x-install-root=$installRoot
    if ($LASTEXITCODE -ne 0) {
        throw "vcpkg install failed."
    }

    $pkgConfExe = Join-Path $installedRoot "tools\pkgconf\pkgconf.exe"
    if (Test-Path $pkgConfExe) {
        $env:PKG_CONFIG = $pkgConfExe
        $env:Path = "$(Split-Path -Parent $pkgConfExe);$env:Path"
    } else {
        Remove-Item Env:PKG_CONFIG -ErrorAction SilentlyContinue
    }

    $pkgConfigPaths = @()
    if ($Configuration -ne "release") {
        $pkgConfigPaths += Join-Path $installedRoot "debug\lib\pkgconfig"
    }
    $pkgConfigPaths += Join-Path $installedRoot "lib\pkgconfig"
    $pkgConfigPaths += Join-Path $installedRoot "share\pkgconfig"
    $existingPkgConfigPaths = $pkgConfigPaths | Where-Object { Test-Path $_ }
    if ($existingPkgConfigPaths.Count -gt 0) {
        $env:PKG_CONFIG_PATH = $existingPkgConfigPaths -join ";"
    } else {
        Remove-Item Env:PKG_CONFIG_PATH -ErrorAction SilentlyContinue
    }

    $env:FFMPEG_DIR = $installedRoot
    $env:LIBOPUS_LIB_DIR = $opusRoot

    & rustup target add $Target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $Target failed."
    }

    $cargoArgs = @("build", "--target", $Target)
    if ($Configuration -eq "release") {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed."
    }

    $stageDir = Join-Path $clientRoot "dist\windows-x64\$Configuration"
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

    Copy-Item (Join-Path $clientRoot "target\$Target\$profile\st-client.exe") $stageDir -Force
    if (Test-Path $dllDir) {
        Get-ChildItem (Join-Path $dllDir "*.dll") | Copy-Item -Destination $stageDir -Force
    }

    Write-Host "Staged Windows build at $stageDir"
} finally {
    Pop-Location
}
