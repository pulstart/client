param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release",
    [string]$Platform = "windows-x64"
)

$ErrorActionPreference = "Stop"
$clientRoot = Split-Path -Parent $PSScriptRoot
$cargoToml = Join-Path $clientRoot "Cargo.toml"
$version = Select-String -Path $cargoToml -Pattern '^version = "(.+)"$' | Select-Object -First 1

if (-not $version) {
    throw "Unable to resolve client version from Cargo.toml."
}

$packageVersion = $version.Matches[0].Groups[1].Value
$stageDir = Join-Path $clientRoot "dist\$Platform\$Configuration"
$exePath = Join-Path $stageDir "st-client.exe"

if (-not (Test-Path $exePath)) {
    throw "Windows stage directory is missing '$exePath'. Run build-windows-msvc.ps1 first."
}

$packageName = "st-client-v$packageVersion-$Platform"
$stagingRoot = Join-Path $clientRoot "dist\staging"
$packageRoot = Join-Path $stagingRoot $packageName
$archivePath = Join-Path $clientRoot "dist\$packageName.zip"

Remove-Item -Recurse -Force $packageRoot -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null

Copy-Item (Join-Path $stageDir "*") $packageRoot -Recurse -Force
@"
This archive contains the Windows x64 build of st-client.

The executable and the runtime DLLs staged by build-windows-msvc.ps1 are included in this
package. No extra setup is required beyond the normal Windows graphics/audio runtime.
"@ | Set-Content (Join-Path $packageRoot "README.txt")

Remove-Item $archivePath -Force -ErrorAction SilentlyContinue
Compress-Archive -Path $packageRoot -DestinationPath $archivePath -Force

Write-Host "Packaged Windows artifact at $archivePath"
