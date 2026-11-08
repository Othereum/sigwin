//! A video's first frame, decoded in a short-lived child process.
//!
//! Media Foundation loads codecs and a good part of the media stack, which must stay out of the
//! long-running app; a broken or hostile file also only takes the child down. The plain video
//! reaches the child through a pipe and stays in its memory: the Windows shell's thumbnails would
//! need a plain file and keep a copy in its thumbnail cache.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};

use anyhow::Context;
use sigwin_thumb::RgbaImage;

const ARG: &str = "--video-frame";
/// Signal's own limit on attachments is lower; anything past this is abnormal input.
const MAX_INPUT: u64 = 256 * 1024 * 1024;
/// Stops decompression bombs: a decoded frame takes 4 bytes per pixel.
#[cfg(windows)]
const MAX_PIXELS: u64 = 100_000_000;
/// A decoder that hangs must not stay around.
const TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
/// Before the frame's pixels, the child writes the video's width, height and seconds, then the
/// frame's width and height.
const HEADER: usize = 20;

/// Call first thing in `main`: returns true if this process was the decoding child.
pub fn run_if_child() -> bool {
    if std::env::args().nth(1).as_deref() != Some(ARG) {
        return false;
    }
    std::thread::spawn(|| {
        std::thread::sleep(TIME_LIMIT);
        std::process::exit(1);
    });
    let mut data = Vec::new();
    let read = std::io::stdin().lock().take(MAX_INPUT + 1).read_to_end(&mut data);
    if read.is_err() || data.len() as u64 > MAX_INPUT {
        return true;
    }
    if let Some(v) = decode(data) {
        let mut out = std::io::stdout().lock();
        for n in [v.size.0, v.size.1, v.seconds, v.frame.width(), v.frame.height()] {
            let _ = out.write_all(&n.to_le_bytes());
        }
        let _ = out.write_all(v.frame.as_raw());
        let _ = out.flush();
    }
    true
}

/// What the child read from a video.
pub struct Decoded {
    /// Upright, at most `PHOTO_THUMB_MAX` on its longer side.
    pub frame: RgbaImage,
    /// The video's own size, as shown (upright).
    pub size: (u32, u32),
    /// How long it plays. 0: the file doesn't say.
    pub seconds: u32,
}

impl Decoded {
    /// Blurhash of the frame. It only has a few components: more pixels would just cost more.
    pub fn blurhash(self) -> Option<String> {
        sigwin_thumb::encode_blurhash(&sigwin_thumb::downscale(self.frame, 64))
    }
}

/// Reads an encrypted video file. `None`: the child couldn't decode it, and wouldn't next time either.
pub fn first_frame(src: &Path) -> anyhow::Result<Option<Decoded>> {
    let file = std::fs::File::open(src).context("open video")?;
    run_child(|stdin| sigwin_vault::decrypt(file, stdin))
}

/// Same for a plain file, one the user is sending.
pub fn first_frame_plain(src: &Path) -> anyhow::Result<Option<Decoded>> {
    let mut file = std::fs::File::open(src).context("open video")?;
    run_child(|mut stdin| std::io::copy(&mut file, &mut stdin).map(drop))
}

/// `feed` writes the plain video, and closes the pipe by dropping it.
fn run_child(feed: impl FnOnce(ChildStdin) -> std::io::Result<()>) -> anyhow::Result<Option<Decoded>> {
    let mut cmd = Command::new(std::env::current_exe().context("own path")?);
    cmd.arg(ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("start decoder")?;
    let stdin = child.stdin.take().context("decoder stdin")?;
    let stdout = child.stdout.take().context("decoder stdout")?;
    // the child reads all of it before it writes anything. A child that died breaks the pipe.
    let fed = feed(stdin);
    let max = crate::view::media::PHOTO_THUMB_MAX as u64;
    let mut out = Vec::new();
    let read = stdout.take(HEADER as u64 + max * max * 4).read_to_end(&mut out);
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = fed {
        // this side's file is bad, or the child went away early, which reading tells apart
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e).context("read video");
        }
    }
    read.context("read decoder output")?;
    Ok(parse(out))
}

fn parse(out: Vec<u8>) -> Option<Decoded> {
    let n = |i: usize| Some(u32::from_le_bytes(out.get(i * 4..i * 4 + 4)?.try_into().ok()?));
    let (size, seconds, w, h) = ((n(0)?, n(1)?), n(2)?, n(3)?, n(4)?);
    Some(Decoded { frame: RgbaImage::from_raw(w, h, out[HEADER..].to_vec())?, size, seconds })
}

/// Whole seconds of a length in 100ns units, the way Media Foundation counts. A short clip isn't
/// shown as nothing at all.
#[cfg_attr(not(windows), allow(dead_code))]
fn whole_seconds(hns: u64) -> u32 {
    if hns == 0 {
        return 0;
    }
    u32::try_from(hns.saturating_add(5_000_000) / 10_000_000).unwrap_or(u32::MAX).max(1)
}

#[cfg(windows)]
fn decode(data: Vec<u8>) -> Option<Decoded> {
    let (img, rotation, seconds) = mf::first_frame(data)?;
    let (w, h) = img.dimensions();
    let size = if matches!(rotation, 90 | 270) { (h, w) } else { (w, h) };
    Some(Decoded { frame: sigwin_thumb::rotated(sigwin_thumb::downscale(img, crate::view::media::PHOTO_THUMB_MAX), rotation), size, seconds })
}

#[cfg(not(windows))]
fn decode(_data: Vec<u8>) -> Option<Decoded> {
    None
}

#[cfg(windows)]
mod mf {
    use std::ffi::c_void;
    use std::ptr::null_mut;

    use sigwin_thumb::RgbaImage;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IStream};
    use windows::Win32::System::Variant::VT_UI8;
    use windows::core::{HRESULT, Interface};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

    const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
    /// `MF_VERSION`; `MFSTARTUP_LITE` leaves out sockets.
    const VERSION: u32 = 0x0002_0070;
    /// A stream can start with empty reads; one that gives nothing for this long is given up on.
    const MAX_READS: u32 = 64;

    /// Looked up at run time: linking these by name would put them in the import table, and every
    /// start of the app would load the media stack.
    unsafe fn symbol<F: Copy>(dll: &str, name: &str) -> Option<F> {
        let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
        let name: Vec<u8> = name.bytes().chain([0]).collect();
        unsafe {
            let lib = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            if lib.is_null() {
                return None;
            }
            let f = GetProcAddress(lib, name.as_ptr())?;
            Some(std::mem::transmute_copy(&f))
        }
    }

    type Out = *mut *mut c_void;

    /// The frame, how many degrees clockwise it should be turned to show, and the video's seconds.
    pub fn first_frame(data: Vec<u8>) -> Option<(RgbaImage, u32, u32)> {
        unsafe {
            let startup: unsafe extern "system" fn(u32, u32) -> HRESULT = symbol("mfplat.dll", "MFStartup")?;
            let on_stream: unsafe extern "system" fn(*mut c_void, Out) -> HRESULT = symbol("mfplat.dll", "MFCreateMFByteStreamOnStream")?;
            let new_attributes: unsafe extern "system" fn(Out, u32) -> HRESULT = symbol("mfplat.dll", "MFCreateAttributes")?;
            let new_type: unsafe extern "system" fn(Out) -> HRESULT = symbol("mfplat.dll", "MFCreateMediaType")?;
            let new_reader: unsafe extern "system" fn(*mut c_void, *mut c_void, Out) -> HRESULT =
                symbol("mfreadwrite.dll", "MFCreateSourceReaderFromByteStream")?;
            let mem_stream: unsafe extern "system" fn(*const u8, u32) -> *mut c_void = symbol("shlwapi.dll", "SHCreateMemStream")?;
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
            startup(VERSION, MFSTARTUP_LITE).ok().ok()?;

            let stream = mem_stream(data.as_ptr(), u32::try_from(data.len()).ok()?);
            if stream.is_null() {
                return None;
            }
            let stream = IStream::from_raw(stream);
            // the stream has its own copy
            drop(data);

            let mut raw = null_mut();
            on_stream(stream.as_raw(), &mut raw).ok().ok()?;
            let bytes = IMFByteStream::from_raw(raw);

            let mut raw = null_mut();
            new_attributes(&mut raw, 1).ok().ok()?;
            let attrs = IMFAttributes::from_raw(raw);
            // converts whatever the decoder gives to RGB
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ok()?;

            let mut raw = null_mut();
            new_reader(bytes.as_raw(), attrs.as_raw(), &mut raw).ok().ok()?;
            let reader = IMFSourceReader::from_raw(raw);
            reader.SetStreamSelection(ALL_STREAMS, false).ok()?;
            reader.SetStreamSelection(FIRST_VIDEO, true).ok()?;

            // phones store the angle beside upright-sensor pixels, and nothing below applies it
            let native = reader.GetNativeMediaType(FIRST_VIDEO, 0).ok()?;
            let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0);
            let seconds = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION).map_or(0, |p| {
                let v = &p.Anonymous.Anonymous;
                if v.vt == VT_UI8 { super::whole_seconds(v.Anonymous.uhVal) } else { 0 }
            });

            let mut raw = null_mut();
            new_type(&mut raw).ok().ok()?;
            let wanted = IMFMediaType::from_raw(raw);
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
            reader.SetCurrentMediaType(FIRST_VIDEO, None, &wanted).ok()?;

            let sample = read_sample(&reader)?;
            // the decoder may have settled on another size while reading
            let current = reader.GetCurrentMediaType(FIRST_VIDEO).ok()?;
            let size = current.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
            let (w, h) = ((size >> 32) as u32, size as u32);
            if w == 0 || h == 0 || w as u64 * h as u64 > super::MAX_PIXELS {
                return None;
            }
            // 1080p is coded as 1088 lines: this says which ones to show
            let (x0, y0, cw, ch) = aperture(&current, w, h);

            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let (mut start, mut len) = (null_mut(), 0);
            buffer.Lock(&mut start, None, Some(&mut len)).ok()?;
            // negative when the rows are stored bottom up, the usual way for RGB
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(w as i32 * 4, |s| s as i32);
            let px = copy_rows(start, len as usize, stride, h, (x0, y0, cw, ch));
            let _ = buffer.Unlock();
            Some((sigwin_thumb::from_bgrx(px?, cw, ch)?, rotation, seconds))
        }
    }

    /// The pixels of an area, top row first. `None` if the buffer is too small for what it claims.
    unsafe fn copy_rows(start: *const u8, len: usize, stride: i32, h: u32, (x0, y0, cw, ch): (u32, u32, u32, u32)) -> Option<Vec<u8>> {
        let abs = stride.unsigned_abs() as usize;
        let row_bytes = cw as usize * 4;
        if abs < (x0 as usize + cw as usize) * 4 || len < abs.checked_mul(h as usize)? {
            return None;
        }
        let all = unsafe { std::slice::from_raw_parts(start, len) };
        let mut px = Vec::with_capacity(row_bytes * ch as usize);
        for y in y0..y0 + ch {
            let row = if stride < 0 { h - 1 - y } else { y } as usize;
            let from = row * abs + x0 as usize * 4;
            px.extend_from_slice(all.get(from..from + row_bytes)?);
        }
        Some(px)
    }

    unsafe fn aperture(t: &IMFMediaType, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let mut area = MFVideoArea::default();
        let buf = unsafe { std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>()) };
        if unsafe { t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, buf, None) }.is_err() {
            return (0, 0, w, h);
        }
        let (x0, y0) = (area.OffsetX.value.max(0) as u32, area.OffsetY.value.max(0) as u32);
        let (cw, ch) = (area.Area.cx.max(0) as u32, area.Area.cy.max(0) as u32);
        if cw == 0 || ch == 0 || x0 + cw > w || y0 + ch > h {
            return (0, 0, w, h);
        }
        (x0, y0, cw, ch)
    }

    unsafe fn read_sample(reader: &IMFSourceReader) -> Option<IMFSample> {
        for _ in 0..MAX_READS {
            let (mut flags, mut sample) = (0u32, None);
            unsafe { reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample)) }.ok()?;
            if sample.is_some() {
                return sample;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return None;
            }
        }
        None
    }
}

#[test]
fn lengths_round_to_seconds() {
    assert_eq!(whole_seconds(0), 0);
    assert_eq!(whole_seconds(1), 1);
    assert_eq!(whole_seconds(14_000_000), 1);
    assert_eq!(whole_seconds(15_000_000), 2);
    assert_eq!(whole_seconds(u64::MAX), u32::try_from(u64::MAX / 10_000_000).unwrap_or(u32::MAX));
}

#[test]
fn parse_needs_the_exact_size() {
    let frame = |w: u32, h: u32, n: usize| {
        let mut v = Vec::new();
        for x in [1920u32, 1080, 75, w, h] {
            v.extend(x.to_le_bytes());
        }
        v.resize(HEADER + n, 7);
        v
    };
    let d = parse(frame(2, 3, 24)).unwrap();
    assert_eq!((d.frame.dimensions(), d.size, d.seconds), ((2, 3), (1920, 1080), 75));
    assert!(parse(frame(2, 3, 23)).is_none());
    assert!(parse(vec![1, 2, 3]).is_none());
    assert!(parse(Vec::new()).is_none());
}
