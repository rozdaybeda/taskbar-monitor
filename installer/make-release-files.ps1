# Collects what a GitHub release publishes, from what build-release.ps1 built.
#
#   taskbar-monitor-<version>.msi, .zip   the installer and the portable copy
#   latest.txt                            what installed copies check for a new version
#   SHA256SUMS.txt                        to check a download by hand
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][string]$Repository,
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$version = $Tag.TrimStart('v')
$installer = Join-Path $root 'target\installer'

if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory $Out | Out-Null
$Out = (Resolve-Path $Out).Path

$msi = "taskbar-monitor-$version.msi"
Copy-Item (Join-Path $installer $msi) $Out
Copy-Item (Join-Path $installer "taskbar-monitor-$version.zip") $Out

# Only this file keeps a fixed name, so that installed copies can always find it. ASCII, as
# PowerShell's own UTF-8 would add a BOM.
$manifest = "version=$version`nurl=https://github.com/$Repository/releases/download/$Tag/$msi`n"
[IO.File]::WriteAllText((Join-Path $Out 'latest.txt'), $manifest, [Text.Encoding]::ASCII)

$sums = Get-ChildItem $Out | ForEach-Object {
    "$((Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
}
[IO.File]::WriteAllText((Join-Path $Out 'SHA256SUMS.txt'), ($sums -join "`n") + "`n", [Text.Encoding]::ASCII)

Get-ChildItem $Out | ForEach-Object { "$($_.Name) $($_.Length)" }
