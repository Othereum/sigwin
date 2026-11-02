# Build, sign and publish a release from this machine (see docs/release.md).
#   .\scripts\release.ps1                       # key: ~\.minisign\sigwin.key
#   .\scripts\release.ps1 -Key D:\keys\sigwin.key
# Needs: cargo, cargo-about, minisign, gh (logged in). minisign asks for the key password.
param([string]$Key = (Join-Path $env:USERPROFILE ".minisign\sigwin.key"))

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Run([string]$what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" }
}

foreach ($tool in "cargo", "cargo-about", "minisign", "gh", "git") {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not found in PATH" }
}
if (-not (Test-Path $Key)) { throw "secret key not found: $Key" }

# release only what is on origin/main
if ((git branch --show-current) -ne "main") { throw "not on main" }
if (git status --porcelain) { throw "working tree is not clean" }
git fetch origin main --quiet; Run "git fetch"
$sha = git rev-parse HEAD
if ($sha -ne (git rev-parse origin/main)) { throw "HEAD differs from origin/main: push or pull first" }

$meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json; Run "cargo metadata"
$v = ($meta.packages | Where-Object name -eq sigwin).version
$tag = "v$v"
gh release view $tag *> $null
if ($LASTEXITCODE -eq 0) { throw "release $tag already exists: bump the version" }
$global:LASTEXITCODE = 0

# the key the app trusts, so a wrong key file fails here and not on users' machines
$pub = [regex]::Match((Get-Content src\update.rs -Raw), 'const PUBLIC_KEY: &str = "([^"]+)"').Groups[1].Value
if (-not $pub) { throw "PUBLIC_KEY not found in src/update.rs" }

$out = New-Item -ItemType Directory -Force target\release-dist
$licenses = Join-Path $out "licenses.html"
cargo about generate -c build-support/about.toml build-support/about.hbs -o $licenses; Run "cargo about"
$env:SIGWIN_LICENSES = (Resolve-Path $licenses).Path

cargo build --release --locked; Run "cargo build"
$exe = Join-Path $out "sigwin.exe"
Copy-Item target\release\sigwin.exe $exe -Force
Remove-Item "$exe.minisig" -ErrorAction SilentlyContinue

# the app accepts only this exact trusted comment for this version
minisign -S -s $Key -m $exe -t "sigwin $v"; Run "minisign sign"
minisign -V -P $pub -m $exe; Run "minisign verify"

$answer = Read-Host "Publish $tag from $($sha.Substring(0, 7))? (y/N)"
if ($answer -ne "y") { Write-Host "Not published. Files are in $out"; exit 1 }

gh release create $tag --target $sha $exe "$exe.minisig" $licenses --title "Sigwin $v" --generate-notes
Run "gh release create"
Write-Host "Published $tag"
