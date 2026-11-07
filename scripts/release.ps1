# Build, sign and publish a release from this machine (see docs/release.md).
#   .\scripts\release.ps1                       # key: ~\.minisign\sigwin.key
#   .\scripts\release.ps1 -Key D:\keys\sigwin.key
# Publishes target\release-notes.md as the release's notes, then removes it (-Notes to change).
# Needs: cargo, cargo-about, minisign, gh (logged in). minisign asks for the key password.
param(
    [string]$Key = (Join-Path $env:USERPROFILE ".minisign\sigwin.key"),
    [string]$Notes = "target\release-notes.md"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

function Run([string]$what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" }
}

foreach ($tool in "cargo", "cargo-about", "minisign", "gh", "git") {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not found in PATH" }
}
if (-not (Test-Path $Key)) { throw "secret key not found: $Key" }
# before the long build; a release never goes out without notes
if (-not (Test-Path $Notes)) { throw "release notes not found: $Notes" }
$Notes = (Resolve-Path $Notes).Path
$notesText = [IO.File]::ReadAllText($Notes, [Text.UTF8Encoding]::new($false))
if (-not $notesText.Trim()) { throw "release notes are empty: $Notes" }

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
# leaves out everything that marks dev builds (build.rs `sigwin_dev`)
$env:SIGWIN_RELEASE = "1"
# keep this machine's paths (user name, repo dir) out of panic locations; the last match wins,
# so the repo comes after the home dir
$repo = (Get-Location).Path
$env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$env:USERPROFILE=~", "--remap-path-prefix=$repo=sigwin") -join [char]0x1f
# same for C code (aws-lc's __FILE__; cc and CMake both take CFLAGS): MSVC's /d1trimfile
$env:CFLAGS = "/d1trimfile:$env:USERPROFILE\ /d1trimfile:$repo\"

cargo build --release --locked; Run "cargo build"
# a dev build (build.rs puts SpecialBuild in its version info) or a leaked path must not ship
$built = (Resolve-Path target\release\sigwin.exe).Path
if ((Get-Item $built).VersionInfo.SpecialBuild) { throw "$built is a dev build: SIGWIN_RELEASE didn't reach build.rs" }
$text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($built))
foreach ($bad in $env:USERPROFILE, $repo) {
    if ($text.IndexOf($bad, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "$built contains '$bad'" }
}
$exe = Join-Path $out "sigwin.exe"
Copy-Item target\release\sigwin.exe $exe -Force
Remove-Item "$exe.minisig" -ErrorAction SilentlyContinue

# the app accepts only this exact trusted comment for this version
minisign -S -s $Key -m $exe -t "sigwin $v"; Run "minisign sign"
minisign -V -P $pub -m $exe; Run "minisign verify"

Write-Host $notesText
$answer = Read-Host "Publish $tag from $($sha.Substring(0, 7)) with these notes? (y/N)"
if ($answer -ne "y") { Write-Host "Not published. Files are in $out"; exit 1 }

# --generate-notes adds the link comparing with the last release below the notes
gh release create $tag --target $sha $exe "$exe.minisig" $licenses --title "Sigwin $v" --notes-file $Notes --generate-notes
Run "gh release create"
Write-Host "Published $tag"
# they were this version's: the next release must not pick them up by accident
Remove-Item $Notes

# gh made the tag on GitHub; bring it here. The release is already out, so failing here isn't fatal
git fetch origin --quiet
if ($LASTEXITCODE -ne 0) { Write-Warning "git fetch failed: run it by hand to get $tag" }
