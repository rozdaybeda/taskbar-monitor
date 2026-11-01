# Builds the release exes and packages them two ways, in target\installer:
#   taskbar-monitor-<version>.msi    per-machine installer, with optional service and driver
#   taskbar-monitor-<version>.zip    portable: runs from anywhere, keeps its data next to the exe
#
#   .\installer\build-release.ps1 -UpdateUrl https://example.com/monitor/latest.txt
#
# -UpdateUrl is where installed copies look for new versions; leave it out to build with
# update checks off. .github\workflows\release.yml publishes what goes at that URL.
#
# -Stage Build and -Stage Package do the two halves, for signing the exes in between.
param(
    [string]$UpdateUrl = '',
    # Overrides the MSI's version only, for trying out upgrades.
    [string]$Version = '',
    [ValidateSet('All', 'Build', 'Package')][string]$Stage = 'All'
)
# Not 'Stop': Windows PowerShell can treat cargo's progress output on stderr as an error.
# Exit codes are checked below instead.
$ErrorActionPreference = 'Continue'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }
if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
}

if ($Stage -ne 'Package') {
    $env:MONITOR_UPDATE_URL = $UpdateUrl
    if (-not (Test-Path "$root\driver\PawnIO_setup.exe")) { & "$root\fetch-pawnio.ps1" }
    & $cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    "Built the exes, update URL: $(if ($UpdateUrl) { $UpdateUrl } else { 'none' })"
}
if ($Stage -eq 'Build') { exit 0 }

# The Rust libraries' licences ask for their notices to ship with the exes.
$cargoAboutVersion = '0.9.3'
& $cargo about --version *> $null
if ($LASTEXITCODE -ne 0) {
    & $cargo install cargo-about --locked --features cli --version $cargoAboutVersion
    if ($LASTEXITCODE -ne 0) { throw 'installing cargo-about failed' }
}
$rustLicenses = "$root\target\release\RUST-LICENSES.txt"
& $cargo about generate --workspace --config installer\about.toml --output-file $rustLicenses installer\about.hbs
if ($LASTEXITCODE -ne 0) { throw 'listing the Rust licences failed' }

dotnet tool restore | Out-Null
dotnet tool run wix -- extension add -g WixToolset.UI.wixext/5.0.2 | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'adding the WiX UI extension failed' }

$out = Join-Path $root 'target\installer'
New-Item -ItemType Directory -Force $out | Out-Null
$msi = Join-Path $out "taskbar-monitor-$Version.msi"
dotnet tool run wix -- build installer\monitor.wxs -arch x64 -ext WixToolset.UI.wixext `
    -d "Version=$Version" `
    -d "ExePath=$root\target\release\monitor.exe" `
    -d "SetupExePath=$root\target\release\monitor-setup.exe" `
    -d "IconPath=$root\assets\icon.ico" `
    -d "LicensesPath=$root\installer\licenses" `
    -d "RustLicensesPath=$rustLicenses" `
    -d "PawnIOSetupPath=$root\driver\PawnIO_setup.exe" `
    -o $msi
if ($LASTEXITCODE -ne 0) { throw 'wix build failed' }

Remove-Item (Join-Path $out "taskbar-monitor-$Version.wixpdb") -ErrorAction SilentlyContinue

$stageFolder = Join-Path $out 'portable-stage'
$folder = Join-Path $stageFolder 'Monitor'
Remove-Item $stageFolder -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $folder | Out-Null
Copy-Item "$root\target\release\monitor.exe" $folder
Copy-Item "$root\target\release\monitor-setup.exe" $folder
Copy-Item "$root\driver\PawnIO_setup.exe" $folder
Copy-Item "$root\installer\install-sensors.bat" $folder
Copy-Item "$root\installer\uninstall-sensors.bat" $folder
Copy-Item "$root\installer\portable.txt" $folder
Copy-Item "$root\installer\licenses\*" $folder
Copy-Item $rustLicenses $folder
$zip = Join-Path $out "taskbar-monitor-$Version.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path $folder -DestinationPath $zip -CompressionLevel Optimal
Remove-Item $stageFolder -Recurse -Force

$size = { param($file) "$([math]::Round((Get-Item $file).Length / 1MB, 1)) MB" }
"Built $msi ($(& $size $msi))"
"Built $zip ($(& $size $zip))"
