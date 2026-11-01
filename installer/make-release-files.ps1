# Collects what a GitHub release publishes, from what build-release.ps1 built.
#
#   Monitor.msi, Monitor-portable.zip   fixed names, so that links to the latest release work
#   latest.txt                          what installed copies check for a new version
#   SHA256SUMS.txt                      to check a download by hand
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

Copy-Item (Join-Path $installer "Monitor-$version.msi") (Join-Path $Out 'Monitor.msi')
Copy-Item (Join-Path $installer "Monitor-$version-portable.zip") (Join-Path $Out 'Monitor-portable.zip')

# The link is to this exact release, so a copy that checks during a later publish still gets
# the file the version number promised. ASCII, as PowerShell's own UTF-8 would add a BOM.
$manifest = "version=$version`nurl=https://github.com/$Repository/releases/download/$Tag/Monitor.msi`n"
[IO.File]::WriteAllText((Join-Path $Out 'latest.txt'), $manifest, [Text.Encoding]::ASCII)

$sums = Get-ChildItem $Out | ForEach-Object {
    "$((Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
}
[IO.File]::WriteAllText((Join-Path $Out 'SHA256SUMS.txt'), ($sums -join "`n") + "`n", [Text.Encoding]::ASCII)

Get-ChildItem $Out | ForEach-Object { "$($_.Name) $($_.Length)" }
