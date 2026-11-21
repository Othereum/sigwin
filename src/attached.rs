//! Reading the files a message is about to send. Pasted images are the only ones Sigwin writes
//! itself, and nothing outside it reads them, so they are sealed in the paste folder
//! (`clip::save_image`); everything else is the user's own file, read as it is. Whatever reads a
//! file to send goes through here.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sigwin_thumb::{ImageResult, Photo};

static PASTE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where pasted images go: set once at start, before anything is pasted.
pub fn set_paste_dir(dir: PathBuf) {
    let _ = PASTE_DIR.set(dir);
}

pub fn paste_dir() -> &'static Path {
    PASTE_DIR.get().map_or(Path::new(""), PathBuf::as_path)
}

fn pasted(path: &Path) -> bool {
    PASTE_DIR.get().is_some_and(|d| path.parent() == Some(d.as_path()))
}

/// The plain size.
pub fn len(path: &Path) -> io::Result<u64> {
    if pasted(path) { sigwin_vault::plain_len(path) } else { Ok(std::fs::metadata(path)?.len()) }
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    if pasted(path) { sigwin_vault::read(path) } else { std::fs::read(path) }
}

/// The plain bytes from the start. A pasted image is opened whole into memory: one the user
/// is sending, and far smaller than the files that are streamed.
pub fn open(path: &Path) -> io::Result<Box<dyn Read + Send>> {
    Ok(if pasted(path) { Box::new(io::Cursor::new(sigwin_vault::read(path)?)) } else { Box::new(std::fs::File::open(path)?) })
}

pub fn photo_size(path: &Path) -> ImageResult<(u32, u32)> {
    if pasted(path) { sigwin_thumb::photo_size(Photo::Bytes(&sigwin_vault::read(path)?)) } else { sigwin_thumb::photo_size(Photo::File(path)) }
}

pub fn photo_facts(path: &Path, max_pixels: u64) -> ImageResult<((u32, u32), Option<String>)> {
    if pasted(path) {
        sigwin_thumb::photo_facts(Photo::Bytes(&sigwin_vault::read(path)?), max_pixels)
    } else {
        sigwin_thumb::photo_facts(Photo::File(path), max_pixels)
    }
}
