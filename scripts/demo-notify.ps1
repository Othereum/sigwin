# Try notifications without a Signal account: the demo app gets a fake incoming message
# every few seconds and runs it through the real notification logic and Windows toasts.
# Click another window so Sigwin isn't focused, then watch / click the toasts.
#   .\scripts\demo-notify.ps1            # every 10s, fast build
#   .\scripts\demo-notify.ps1 -Every 5 -DebugBuild
param([int]$Every = 10, [switch]$DebugBuild)

$exe = Join-Path $PSScriptRoot ("..\target\" + ($(if ($DebugBuild) { "debug" } else { "fast" })) + "\sigwin.exe")
if (-not (Test-Path $exe)) { throw "build first: $exe not found" }
# a scratch data folder: never the real account's
$data = Join-Path ([IO.Path]::GetTempPath()) "sigwin-demo-notify"
New-Item -ItemType Directory -Force $data | Out-Null
$env:SIGWIN_DATA = $data
$env:SIGWIN_DEMO = "1"
$env:SIGWIN_DEMO_NOTIFY = "$Every"
$env:RUST_LOG = "info,sigwin=debug"
& $exe
