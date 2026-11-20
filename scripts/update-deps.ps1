# Update Cargo.lock to the newest compatible versions, right after a release (see docs/release.md).
#   .\scripts\update-deps.ps1
# Keeps every [patch] in use, builds, runs all tests and leaves Cargo.lock uncommitted for a look at
# the real app. Git deps (presage, libsignal-service, rusqlite) stay on their pinned revs.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$repo = (Get-Location).Path

function Run([string]$what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" }
}

# the commit after this should hold the lock update and nothing else
if (git status --porcelain) { throw "working tree is not clean" }

# A newer registry release of a patched crate wins over the vendor copy: the build still succeeds,
# without the patch. Cargo.lock lists such patches under [[patch.unused]].
function Get-UnusedPatches {
    $lock = Get-Content Cargo.lock -Raw
    [regex]::Matches($lock, '(?m)^\[\[patch\.unused\]\]\r?\nname = "([^"]+)"\r?\nversion = "([^"]+)"') |
        ForEach-Object { [pscustomobject]@{ Name = $_.Groups[1].Value; Version = $_.Groups[2].Value } }
}
if (Get-UnusedPatches) { throw "a [patch] is already unused before updating: fix Cargo.lock first" }

$out = cargo update --verbose 2>&1 | ForEach-Object { "$_" }
Run "cargo update"

foreach ($p in Get-UnusedPatches) {
    $lock = Get-Content Cargo.lock -Raw
    $picked = [regex]::Matches($lock, "(?m)^name = `"$([regex]::Escape($p.Name))`"\r?\nversion = `"([^`"]+)`"\r?\nsource = `"registry") |
        ForEach-Object { $_.Groups[1].Value }
    foreach ($v in $picked) {
        Write-Host "Keeping patched $($p.Name) $($p.Version) (update picked $v)"
        cargo update -p "$($p.Name)@$v" --precise $p.Version
        # something else now needs the newer release: the vendor copy has to be rebased onto it
        Run "pinning $($p.Name) to its patched version"
    }
}
$still = Get-UnusedPatches
if ($still) {
    throw "patches still unused: $(($still | ForEach-Object { "$($_.Name) $($_.Version)" }) -join ', '). Rebase them (vendor/PATCHES.md)"
}

$changed = $out | Where-Object { $_ -match '^\s*(Updating|Adding|Removing|Downgrading) ' -and $_ -notmatch 'crates\.io index|git repository' }
$behind = $out | Where-Object { $_ -match '^\s*Unchanged .*\(available: ' }
if (-not $changed) { Write-Host "Everything is up to date."; exit 0 }

# a running dev build locks its exe; installed or other copies are left alone
Get-Process sigwin -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -like "$repo\target\*" } | Stop-Process -Force
cargo build --profile fast; Run "cargo build"
cargo test --profile fast; Run "cargo test"

Write-Host "`nUpdated:"
$changed | ForEach-Object { Write-Host "  $($_.Trim())" }
if ($behind) {
    Write-Host "`nNewer releases outside the Cargo.toml requirements (bump by hand if wanted):"
    $behind | ForEach-Object { Write-Host "  $($_.Trim())" }
}
Write-Host "`nNext: run target\fast\sigwin.exe on the real account, then commit Cargo.lock as 'Update dependencies'."
