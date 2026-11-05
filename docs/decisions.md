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
