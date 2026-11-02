# Check that a minisign secret key file still signs for the key the app trusts.
#   .\scripts\check-key.ps1                      # ~\.minisign\sigwin.key
#   .\scripts\check-key.ps1 -Key E:\backup\sigwin.key
# Signs a temp file (asks for the key password) and verifies it against PUBLIC_KEY in src/update.rs.
param([string]$Key = (Join-Path $env:USERPROFILE ".minisign\sigwin.key"))

$ErrorActionPreference = "Stop"

if (-not (Get-Command minisign -ErrorAction SilentlyContinue)) { throw "minisign not found in PATH" }
if (-not (Test-Path $Key)) { throw "key file not found: $Key" }

$src = Join-Path $PSScriptRoot "..\src\update.rs"
$pub = [regex]::Match((Get-Content $src -Raw), 'const PUBLIC_KEY: &str = "([^"]+)"').Groups[1].Value
if (-not $pub) { throw "PUBLIC_KEY not found in src/update.rs" }

$tmp = Join-Path ([IO.Path]::GetTempPath()) "sigwin-keycheck.txt"
try {
    "keycheck" | Set-Content $tmp
    Remove-Item "$tmp.minisig" -ErrorAction SilentlyContinue
    minisign -S -s $Key -m $tmp -t "keycheck"
    if ($LASTEXITCODE -ne 0) { throw "signing failed: wrong password or unreadable key file" }
    minisign -V -P $pub -m $tmp
    if ($LASTEXITCODE -ne 0) { throw "signature does NOT verify against the app's PUBLIC_KEY: this is a different key" }
    Write-Host "OK: $Key signs for the app's PUBLIC_KEY"
} finally {
    Remove-Item $tmp, "$tmp.minisig" -ErrorAction SilentlyContinue
}
