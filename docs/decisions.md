# Decisions and measurements

## Why the software renderer (measured 2026-11-01, demo data, NVIDIA GPU)
| renderer | private | working set | CPU for 80 scroll notches | color emoji | ✓✕➤ fallback |
|---|---|---|---|---|---|
| software (software-only build) | **8.4MB** | 28MB | **750ms** | no (monochrome) | no (blank) |
| femtovg (OpenGL) | 212MB | 157MB | 2328ms | – | – |
| skia | 388MB | 252MB | 1641ms | yes | no (tofu) |

GPU renderers load the graphics driver into the process (nvoglv64, d3d12): they blow the 100MB budget by themselves, and they are not cheaper on CPU either. Only compiling the GPU renderers in, even when running software, already costs ~54MB private. The `renderer-femtovg` / `renderer-skia` cargo features exist only for re-measuring (`SLINT_BACKEND=winit-skia`); never enable them by default.

## Color emoji: DirectWrite path removed
A DirectWrite path (3D COLRv1 style) was tried and removed. In-process it kept ~20MB of font caches (+18MB for 16 emoji, only 6MB returned on drop), it needed a child process plus a PNG cache, and it mixed two emoji styles on screen. Now emoji are plain text drawn by the patched software renderer in one flat style.
