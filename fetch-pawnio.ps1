# Downloads the official PawnIO setup and the sensor modules into driver\. The build embeds
# the modules, and the packages ship the setup. They aren't kept in the repo. Run this once
# after cloning, and in CI.
#
# Versions and hashes are pinned, so a build can't pick up anything else. To update, change
# them here and check the new release.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = 'Tls12'

$setupUrl = 'https://github.com/namazso/PawnIO.Setup/releases/download/2.2.0/PawnIO_setup.exe'
$setupHash = '1f519a22e47187f70a1379a48ca604981c4fcf694f4e65b734aaa74a9fba3032'
$modulesUrl = 'https://github.com/namazso/PawnIO.Modules/releases/download/0.2.11/release_0_2_11.zip'
$modulesHash = '43608cb89bc84247fef1368a139013f7d043e17db6d6c8dfc9b46bf0905a81f4'
$modules = 'AMDFamily17.bin', 'IntelMSR.bin'

function Get-Verified($url, $hash, $path) {
    Invoke-WebRequest $url -OutFile $path -UseBasicParsing
    $actual = (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $hash) {
        Remove-Item $path
        throw "$url has hash $actual, expected $hash"
    }
}

$out = Join-Path $PSScriptRoot 'driver'
New-Item -ItemType Directory -Force $out | Out-Null

$setup = Join-Path $out 'PawnIO_setup.exe'
Get-Verified $setupUrl $setupHash $setup
"Wrote $setup"

$zip = Join-Path $out 'modules.zip'
$unpacked = Join-Path $out 'modules'
Get-Verified $modulesUrl $modulesHash $zip
Expand-Archive $zip -DestinationPath $unpacked -Force
foreach ($module in $modules) {
    Copy-Item (Join-Path $unpacked $module) $out
    "Wrote $(Join-Path $out $module)"
}
Remove-Item $zip
Remove-Item $unpacked -Recurse
