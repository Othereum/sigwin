# Headless demo run: scrolls the message list, one shot per step, and joins the shots into one image.
#   run.ps1 -Out <dir> -Up 20 -Down 20 -Env @{ SIGWIN_DEMO_OLDER = "20"; SIGWIN_DEMO_CAP = "120" }
#   run.ps1 -Out <dir> -Steps @("wait 500", "click 600 80", "wait 1500", "shot <dir>\pin.png")
# Prints the path of the image. See SKILL.md.
param(
    [Parameter(Mandatory)][string]$Out,
    [hashtable]$Env = @{},
    [int]$Up = 0,
    [int]$Down = 0,
    [int]$Notches = 8,
    [string[]]$Steps = @(),
    [string]$Exe = "target\fast\sigwin.exe",
    [switch]$Full
)
$ErrorActionPreference = "Stop"

$Exe = (Resolve-Path $Exe).Path
Remove-Item -LiteralPath $Out -Recurse -Force -ErrorAction SilentlyContinue
$Out = (New-Item -ItemType Directory -Force $Out).FullName
$shots = Join-Path $Out "shots"
New-Item -ItemType Directory -Force $shots | Out-Null

$all = @("wait 500", "shot $shots\000_start.png") + $Steps
if ($Up -gt 0) {
    foreach ($i in 1..$Up) { $all += "wheel 650 350 $Notches", "wait 200", ("shot $shots\1{0:d2}_up.png" -f $i) }
}
if ($Down -gt 0) {
    foreach ($i in 1..$Down) { $all += "wheel 650 350 -$Notches", "wait 200", ("shot $shots\2{0:d2}_down.png" -f $i) }
}
$stepFile = Join-Path $Out "steps.txt"
$all | Set-Content $stepFile

# a scratch data dir always: without it a demo reaches the real account's
$env:SIGWIN_DATA = Join-Path $Out "data"
$env:SIGWIN_DEMO = "1"
# debug logs would print every step into the output
$env:RUST_LOG = "off"
$env:SIGWIN_HEADLESS = "@$stepFile"
foreach ($k in $Env.Keys) { Set-Item "Env:$k" $Env[$k] }

$p = Start-Process $Exe -PassThru -NoNewWindow
if (-not $p.WaitForExit(180000)) { $p.Kill(); throw "timed out" }
if ($p.ExitCode -ne 0) { throw "exit code $($p.ExitCode)" }

$sheet = Join-Path $Out "sheet.png"
& (Join-Path $PSScriptRoot "sheet.ps1") -Dir $shots -Output $sheet -Full:$Full
$sheet
