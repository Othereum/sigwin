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

Large images (synthetic JPEG, headless, opaque): 100MP peaks at 765MB and holds 383MB, 200MP at 1528MB and 764MB; all of it returns on close. Frame times don't grow with the image. `VIEW_MAX_PIXELS` stays at 100MP anyway: such photos are rare here, and the limit still stops decompression bombs (a small PNG can claim 17GB). Transparent images are worse while zoomed: `fast_image_resize` premultiplies the whole source for every patch (200MP: 225-370ms and about 860MB more).

The viewer keeps the original from the moment it opens instead of decoding it again on the first zoom. Whoever opens an image usually zooms it, so waiting would decode most images twice and delay the first smoothing by as long as a decode (200MP: 1.3s), to save memory that returns on close anyway.

`fast_image_resize` (5.5.0 and 6.1.0) has a crash reachable from safe code: when enlarging with a kernel that is zero at whole numbers (Catmull-Rom), a first output pixel centered exactly on a source pixel trims its bound past the next pixel's, `b.start -= x_first` underflows and it reads out of bounds. `viewer::patch_crop` can't produce such a crop, and a test holds it to that.

Going to another image of an album decodes it on the spot. 12MP in the `fast` build took 220ms: 50ms decoding and fitting, 118ms copying into the UI's buffer and 50ms freeing the previous image, those two slow because the main crate is unoptimized there. `decode_view` now fills the UI's buffer itself, in the optimized crate. Measured on a synthetic JPEG with everything optimized, that barely changes the time (73ms to 70ms) but lowers the peak (103MB to 84MB). Decoding the neighbouring images ahead would remove the rest of the wait, at about 96MB more while the viewer is open; not done, it's fast enough.

## Link previews
A preview is made by the sender's app and travels in the message, so showing a received one contacts nothing but Signal. Making one means the sender contacts the site, which sees their IP address. Signal Desktop does the same, without a proxy: its content proxy isn't used for this, and the Storage Service field for proxied previews is retired. So it's a setting, on by default as in Signal, and receiving never fetches: a link someone else sends must not make us reveal anything.

Rejected: fetching at send time only. The user wouldn't see or be able to remove what goes out. Letting presage do it: no switch, a blocked send, the url in its log. A preview with no fields to stop presage's fetch: Signal iOS requires the url field and drops the whole message without it.

## Video thumbnails
A message carries a video's blurhash and size at most, so its real picture has to come from the file. Signal Desktop does the same: it downloads the video and takes a frame.

Whole downloads with a size limit, not partial ones. The CDN seems to take `Range` and CBC can start anywhere, so the index and first frame alone would do. That needs ranged reads the vendored service doesn't offer, a decoder asking for bytes as it goes, and either `incrementalMac` or decoding unverified bytes when the sender left it out. Too much for what it saves. Automatic downloads are limited because `get_attachment` holds the whole file in memory while it arrives; `view::media::auto_fetches_video` is the one place that decides, for a later setting.

Decoding: Media Foundation in a child, fed through a pipe. Rejected: the shell's thumbnails (need a plain file and keep a copy in its thumbnail cache), bundling ffmpeg (an extra runtime and size), a Rust H.264 decoder with a demuxer (H.264 only, and it adds size). A codec Windows lacks leaves the blurhash, as does anything over the limit. Its functions are looked up at run time, because linking them would load the media stack on every start of the app.

The child takes 27 to 69MB at its peak for 0.2 to 3.5MB test videos, about 90ms, and exits. Larger videos weren't measured; its input is held in memory, so expect the file size on top. The first frame is what's shown, even if it is black.

Sending: the same child reads the file being sent for its size and blurhash, so other clients can draw the video before fetching it. One it can't decode goes without them instead of failing the send. Photos get the same, decoded in the app since it's the `image` crate, on a blocking thread. A photo's size is sent as shown, with EXIF applied, because the file keeps its EXIF.

Labels on pictures: a video's length comes from the child along with its frame, because a message doesn't carry it, so it can only show once the video is here. Until then the picture shows the size the message does carry. The length is kept encrypted next to the frame. A frame cached before lengths were kept reads its video once more, if that is still here.

Download progress on videos: `Manager::get_attachment` reads the whole file before returning, so videos go through libsignal-service's public `PushService` with the same checks and the same user agent. A vendor patch to presage would have saved those few lines at the cost of a hunk on every upgrade. The client is made per download, since one kept around would add to idle memory. Photos and files still use presage: photos download by themselves in moments, and a file line has nowhere to show it.

A click on an undownloaded video only downloads it, and another click plays it. Rejected: playing it when the download ends, which can come minutes later, over whatever the user is doing by then, taking the focus and making noise, with others perhaps looking at the screen. A video stopped by a click isn't downloaded by itself again until the app restarts; that is kept in memory only.

## Sticker and avatar resampling (measured 2026-11-06, synthetic images, heap peak without the source)
`fit` moved from `image`'s resize in f32 to `fast_image_resize`:
| job | `image`, f32 | fir |
|---|---|---|
| sticker 512 to 140 | 5.1ms / 5.7MB | 0.53ms / 1.5MB |
| sticker 112 to 140 | 0.46ms / 0.8MB | 0.10ms / 0.2MB |
| photo 4000x3000 to 400 | 140ms / 213MB | 20ms / 54MB |

Eight bits per channel cost little: over white and black the results differ by 0.3 of 255 on average and 10 at most, along edges, not visible side by side, with no dark fringes.

`decode_thumb` keeps `thumbnail`: at 4000x3000 to 400 it takes 14ms and 1MB, fir 8.5ms and 5.7MB without premultiplying. It runs on its own for every image received, so memory decides.
