<#
Regenerates the README's pictures in docs/media from the demo data (SIGWIN_DEMO_REAL), headless:
no window, no account, nothing personal. Run it after a UI change that shows in them:

    pwsh scripts/readme-media.ps1
    pwsh scripts/readme-media.ps1 -Only ime,link

Each picture comes in English and Korean (README.md / README.ko.md), light and dark (the README
picks by the reader's device or browser setting, not by GitHub's own theme). All but the first are
animated PNGs:
    hero-*        the main window
    readers-*     a message sent to a group, read by one member after another
    link-*        a link sent before its preview arrived still gets it
    viewer-*      a photo opened from a small window, zoomed, closed
    ime-*         @mention candidates narrowing while a Korean syllable is being composed, and
                  found by initials (Korean typing: one picture for both READMEs)
    notify-*      (still) a chat's own notification settings

memory-light.png / memory-dark.png (the README's memory comparison) are real measurements in Task
Manager, not made here.

A running sigwin.exe built from this tree locks it and the build fails: quit it first.
#>
param(
    # the shots' scale: sharp on high-DPI screens, at a cost in file size
    [double]$Scale = 1.5,
    # only these pictures (hero, link, readers, viewer, ime, notify), so the others stay as they are
    [string[]]$Only
)
function Want($scene) { -not $Only -or $Only -contains $scene }
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$out = Join-Path $root 'docs\media'
$work = Join-Path ([IO.Path]::GetTempPath()) 'sigwin-readme-media'

Push-Location $root
try {
    cargo build --bin sigwin --example readme_media
    if ($LASTEXITCODE) { throw 'build failed' }
} finally {
    Pop-Location
}
$exe = Join-Path $root 'target\debug\sigwin.exe'
$tool = Join-Path $root 'target\debug\examples\readme_media.exe'

if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force $work, $out | Out-Null

function Px([double]$v, [double]$s = $Scale) { [int][math]::Round($v * $s) }
function Rect($x, $y, $w, $h) { '{0},{1},{2},{3}' -f (Px $x), (Px $y), (Px $w), (Px $h) }

# One headless run of the demo on its own scratch data dir.
function Invoke-Demo($lang, $theme, [string[]]$steps, [double]$s = $Scale) {
    # fresh every run: a draft left by one scene would move its chat up in the next
    $data = Join-Path $work "data-$lang-$theme"
    if (Test-Path $data) { Remove-Item -Recurse -Force $data }
    $file = Join-Path $work 'steps.txt'
    Set-Content -Encoding utf8 $file (@("scale $s") + $steps)
    $saved = @{}
    $vars = @{
        SIGWIN_DATA = $data; SIGWIN_DEMO = '1'; SIGWIN_DEMO_REAL = '1'; SIGWIN_LANG = $lang
        SIGWIN_HEADLESS_THEME = $theme; SIGWIN_HEADLESS = "@$file"; RUST_LOG = 'warn'
    }
    foreach ($k in $vars.Keys) {
        $saved[$k] = [Environment]::GetEnvironmentVariable($k)
        [Environment]::SetEnvironmentVariable($k, $vars[$k])
    }
    try {
        # a GUI exe: `&` wouldn't wait for it
        $p = Start-Process $exe -NoNewWindow -Wait -PassThru
        if ($p.ExitCode) { throw "headless run failed ($lang, $theme)" }
    } finally {
        foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
    }
}

function Invoke-Tool([string[]]$a, [double]$s = $Scale) {
    & $tool $s @a
    if ($LASTEXITCODE) { throw "readme_media $($a -join ' ') failed" }
}


# Frames of an animation, NNNN-<ms>.png: shown for <ms>. `Frame2` also shoots the image viewer
# (NNNN-<ms>-2.png) and leaves the steps on window 2. Headless notes in each shot where the
# window and the pointer are on its screen; the tool puts them together from that.
$script:frames = ''
$script:n = 0
function Start-Frames($dir) {
    $script:frames = $dir
    $script:n = 0
    New-Item -ItemType Directory -Force $dir | Out-Null
}
function Frame($ms) {
    $s = "shot $script:frames\{0:d4}-$ms.png" -f $script:n
    $script:n++
    $s
}
function Frame2($ms) {
    $base = "$script:frames\{0:d4}-$ms" -f $script:n
    $script:n++
    'window 1', "shot $base.png", 'window 2', "shot $base-2.png"
}
# The pointer slides from one point to another as a hand moves it, a frame per step.
function Glide($x0, $y0, $x1, $y1, $count = 8) {
    foreach ($i in 1..$count) {
        # eased: fast in the middle, slow at both ends
        $t = (1 - [math]::Cos([math]::PI * $i / $count)) / 2
        'move {0:0} {1:0}' -f ($x0 + ($x1 - $x0) * $t), ($y0 + ($y1 - $y0) * $t)
        'wait 20'
        Frame 30
    }
}
# Types `text` a character at a time, a frame after each (emoji are one character; a space is a key).
function Type-Slowly($text) {
    $e = [Globalization.StringInfo]::GetTextElementEnumerator($text)
    while ($e.MoveNext()) {
        $c = $e.GetTextElement()
        $(if ($c -eq ' ') { 'key space' } else { "type $c" }), 'wait 40', (Frame 70)
    }
}
# Frames every 100ms while the demo answers, then the end held.
function Watch($count, $hold) {
    foreach ($i in 1..$count) { 'wait 80', (Frame 100) }
    'wait 100', (Frame $hold)
}
function Click-Composer {
    'click 600 725', 'wait 200', (Frame 1200)
}

$text = @{
    # typed before the link is pasted
    link = @{ en = 'Got us tickets! '; ko = '표 예매했어요! ' }
    group = @{ en = "I'll bring flowers too 💐"; ko = '꽃도 사 갈게요 💐' }
}
# made up; presage gives reserved names like example.com no preview at all
$url = 'https://lumen-gallery.art/tickets'

foreach ($lang in 'en', 'ko') {
    foreach ($theme in 'light', 'dark') {
        $name = "$lang-$theme"
        $shots = Join-Path $work $name
        New-Item -ItemType Directory -Force $shots | Out-Null

        if (Want hero) {
            Invoke-Demo $lang $theme @('size 1000 760', 'wait 800', "shot $shots\hero.png")
            Invoke-Tool frame, "$shots\hero.png", "$out\hero-$name.png"
        }

        # a link pasted and sent at once, before its preview could show
        if (Want link) {
            Start-Frames "$shots\link"
            $steps = @('size 1000 760', 'wait 800')
            $steps += Click-Composer
            $steps += Type-Slowly $text.link[$lang]
            $steps += "type $url", 'wait 400', (Frame 500), 'key Enter'
            $steps += Watch 24 3000
            Invoke-Demo $lang $theme $steps
            Invoke-Tool apng, "$shots\link", "$out\link-$name.png", "crop=$(Rect 300 260 700 500)"
        }

        # the group reads our message: the photos move down to it one by one
        if (Want readers) {
            Start-Frames "$shots\readers"
            $steps = @('size 1000 760', 'wait 800', 'click 150 150', 'wait 600')
            $steps += Click-Composer
            $steps += Type-Slowly $text.group[$lang]
            $steps += 'key Enter'
            $steps += Watch 40 3000
            Invoke-Demo $lang $theme $steps
            Invoke-Tool apng, "$shots\readers", "$out\readers-$name.png", "crop=$(Rect 300 200 700 560)"
        }

        # a photo from a small window opens around the pointer, zooms with the wheel around it,
        # closes on a click
        if (Want viewer) {
            Start-Frames "$shots\viewer"
            $steps = @(
                'screen 1280 800', 'size 760 560', 'at 40 40', 'wait 800', 'wheel 500 300 3',
                # the pointer on the header, so no row shows hovered; the date pill gone
                'move 620 30', 'wait 1500', (Frame 800)
            )
            $steps += Glide 620 30 500 220 10
            $steps += 'wait 300', (Frame 400)
            # the viewer's window comes with its image: give the demo time to paint one
            $steps += 'click', 'wait 1500'
            $steps += Frame2 1000
            foreach ($i in 1..2) {
                $steps += 'wheel 1'
                foreach ($j in 1..3) { $steps += 'wait 60'; $steps += Frame2 100 }
            }
            $steps += 'wait 500'; $steps += Frame2 1200
            $steps += 'click', 'window 1', 'wait 300', (Frame 500)
            $steps += Glide 500 220 620 30 10
            $steps += 'wait 300', (Frame 1500)
            # shown at about half size in the README: no need for more pixels
            Invoke-Demo $lang $theme $steps 1
            Invoke-Tool -s 1 apng, "$shots\viewer", "$out\viewer-$name.png", cursor
        }

        # @mention candidates narrow while a Korean syllable is still being composed, even where
        # its final consonant will start the next one (잇 → 이서), and one is picked right there;
        # then by initials alone. Korean typing only, so one picture serves both READMEs.
        if ((Want ime) -and $lang -eq 'ko') {
            Start-Frames "$shots\ime"
            $steps = @('size 1000 760', 'wait 800', 'click 150 278', 'wait 600')
            $steps += Click-Composer
            $steps += 'type @', 'wait 100', (Frame 400)
            foreach ($c in 'ㅇ', '이') { $steps += "ime $c", 'wait 100', (Frame 500) }
            $steps += 'ime 잇', 'wait 100', (Frame 1000)
            $steps += 'commit 잇', 'key Enter', 'wait 200', (Frame 1000)
            # ㅇ → 이민준, 이서연, 최유나 (유); ㅇㅁ → 이민준
            $steps += 'type @', 'wait 100', (Frame 400)
            $steps += 'ime ㅇ', 'wait 100', (Frame 500), 'commit ㅇ'
            $steps += 'ime ㅁ', 'wait 100', (Frame 500), 'commit ㅁ'
            $steps += 'ime ㅈ', 'wait 100', (Frame 1000), 'commit ㅈ'
            $steps += 'key Enter', 'wait 200', (Frame 3000)
            Invoke-Demo $lang $theme $steps
            Invoke-Tool apng, "$shots\ime", "$out\ime-$theme.png", "crop=$(Rect 300 560 700 200)"
        }

        # a chat's own notification settings
        if (Want notify) {
            Invoke-Demo $lang $theme @(
                'size 1000 760', 'wait 800', 'click 150 150', 'wait 600', 'click 970 28', 'wait 500',
                'click 810 276', 'wait 600', "shot $shots\notify.png"
            )
            Invoke-Tool frame, "$shots\notify.png", "$out\notify-$name.png"
        }
    }
}
Get-ChildItem $out | Format-Table Name, @{ n = 'KB'; e = { [math]::Round($_.Length / 1KB) } }
