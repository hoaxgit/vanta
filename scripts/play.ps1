# vanta launcher: runs the game for normal play (no scripted presses, no capture).
param(
    [Parameter(Mandatory = $true)][string]$Game,
    [string]$Cache = "$env:LOCALAPPDATA\vanta\cache",
    [string]$Data = "$env:LOCALAPPDATA\vanta\data",
    [string]$Exe = "$PSScriptRoot\..\target\release\vanta.exe"
)
foreach ($v in "VANTA_KEYS", "VANTA_CAPTURE", "VANTA_CAPTURE_FRAMES", "VANTA_DUMP_TARGETS") {
    Remove-Item "Env:$v" -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Force $Cache, $Data | Out-Null
$env:VANTA_CACHE_DIR = $Cache
$env:VANTA_DATA_DIR = $Data
& $Exe run $Game --execute 2>&1 | Out-File "$env:TEMP\vanta.log" -Encoding utf8
Write-Host "Exited with $LASTEXITCODE; log: $env:TEMP\vanta.log"
