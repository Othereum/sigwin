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
if (git status --porcelain --untracked-files=no) { throw "working tree is not clean" }

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

# Newer releases cargo update can't take need a Cargo.toml change and maybe code changes, so most
# are left alone. Worth it when only our own crates hold the old version while the build already
# has another one: moving to that one drops a copy from the build.
function Show-Behind {
    $behind = $out | ForEach-Object {
        if ($_ -match '^\s*Unchanged (\S+) v(\S+) \(available: v([^)]+)\)') {
            [pscustomobject]@{ Name = $Matches[1]; Version = $Matches[2]; Latest = $Matches[3] }
        }
    }
    if (-not $behind) { return }
    $members = (cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages.name
    $pkgs = (Get-Content Cargo.lock -Raw) -split '\r?\n\[\[package\]\]\r?\n' | Select-Object -Skip 1 | ForEach-Object {
        $deps = if ($_ -match '(?s)dependencies = \[(.*?)\]') { [regex]::Matches($Matches[1], '"([^"]+)"') | ForEach-Object { $_.Groups[1].Value } }
        [pscustomobject]@{
            Name = [regex]::Match($_, '(?m)^name = "([^"]+)"').Groups[1].Value
            Version = [regex]::Match($_, '(?m)^version = "([^"]+)"').Groups[1].Value
            Deps = @($deps)
        }
    }
    $worth = @(); $rest = @()
    foreach ($b in $behind) {
        $others = @($pkgs | Where-Object { $_.Name -eq $b.Name -and $_.Version -ne $b.Version } | ForEach-Object Version)
        # with several versions in the lock, every reference names its version
        $users = @($pkgs | Where-Object { $_.Deps | Where-Object { $_ -match "^$([regex]::Escape($b.Name)) $([regex]::Escape($b.Version))( |$)" } } | ForEach-Object Name)
        if ($others -and $users -and -not ($users | Where-Object { $_ -notin $members })) {
            $worth += "$($b.Name) $($b.Version) -> $($others -join ' or ') (held only by $(($users | Sort-Object -Unique) -join ', '))"
        } else { $rest += $b.Name }
    }
    if ($worth) {
        Write-Host "`nBumping these in Cargo.toml would drop a copy from the build:"
        $worth | ForEach-Object { Write-Host "  $_" }
    }
    if ($rest) { Write-Host "`nNewer releases not taken (leave them unless a fix or feature is needed): $($rest -join ', ')" }
}

if (-not $changed) { Write-Host "Everything is up to date."; Show-Behind; exit 0 }

# a running dev build locks its exe; installed or other copies are left alone
Get-Process sigwin -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -like "$repo\target\*" } | Stop-Process -Force
cargo build --profile fast; Run "cargo build"
cargo test --profile fast; Run "cargo test"

Write-Host "`nUpdated:"
$changed | ForEach-Object { Write-Host "  $($_.Trim())" }
Show-Behind
Write-Host "`nNext: run target\fast\sigwin.exe on the real account, then commit Cargo.lock as 'Update dependencies'."
