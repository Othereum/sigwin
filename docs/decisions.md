# Decisions and measurements

## Memory target: idle, not peak
The target is idle private memory (30MB), not a fixed peak. A peak cap counts only this process: it pushed a large photo out to a default app, which can use more memory than decoding it here, so the system as a whole lost. That reasoning only covers what the user opens and what would otherwise go to an outside app, and only if the memory comes back on close, because Sigwin keeps running and an outside app exits. Automatic work has no such alternative and stays small.

## Why the software renderer (measured 2026-11-01, demo data, NVIDIA GPU)
| renderer | private | working set | CPU for 80 scroll notches | color emoji | ✓✕➤ fallback |
|---|---|---|---|---|---|
| software (software-only build) | **8.4MB** | 28MB | **750ms** | no (monochrome) | no (blank) |
| femtovg (OpenGL) | 212MB | 157MB | 2328ms | – | – |
| skia | 388MB | 252MB | 1641ms | yes | no (tofu) |

GPU renderers load the graphics driver into the process (nvoglv64, d3d12): they blow the memory target by themselves, and they are not cheaper on CPU either. Only compiling the GPU renderers in, even when running software, already costs ~54MB private. The `renderer-femtovg` / `renderer-skia` cargo features exist only for re-measuring (`SLINT_BACKEND=winit-skia`); never enable them by default.

## Color emoji: DirectWrite path removed
A DirectWrite path (3D COLRv1 style) was tried and removed. In-process it kept ~20MB of font caches (+18MB for 16 emoji, only 6MB returned on drop), it needed a child process plus a PNG cache, and it mixed two emoji styles on screen. Now emoji are plain text drawn by the patched software renderer in one flat style.

## Image viewer resampling (measured 2026-11-06, synthetic JPEG, 16 threads)
`fast_image_resize` (integer SIMD, single thread) over `image`:
| job | `thumbnail` | `image` Lanczos3 | fir CatmullRom |
|---|---|---|---|
| 12MP to 1440x1080 | 25ms / 5MB | 122ms / 71MB | 12ms / 22MB |
| 48MP to 1440x1080 | 62ms / 5MB | 310ms / 137MB | 32ms / 39MB |
| 640x360 to 1080p | 24ms / 7MB | 43ms / 18MB | 3ms / 10MB |

Enlarging against an analytic chart: `thumbnail` 12dB (visible stairs), fir Bilinear 22dB, CatmullRom 26dB, Lanczos3 28dB with visible halos. So Lanczos3 down, CatmullRom up. It adds 0.6MB to the release exe. Sixteen threads gain little (48MP: 32ms to 22ms); decoding is the slow step and has no thread option. Transparent images need premultiplying: 68MB instead of 22MB for the first job.

Smoothing only what's visible keeps memory and time bounded by the window, whatever the zoom: resampling the whole image takes 411MB for a 12MP photo at 300%. Rejected while dragging:
- A patch with a margin, rebuilt when the drag ends: its edge shows once the drag passes the margin.
- Rebuilding the patch during the drag: visibly choppy. At a 2560x1392 window a patch with margin (8MP) took 86ms to make and the frame showing it 43ms instead of 10.

Bare pixels while dragging, smooth when it stops, look fine: motion hides the difference. Wheel notches in one burst were at most 65ms apart and slow single notches 400ms or more, hence the 100ms wait.
