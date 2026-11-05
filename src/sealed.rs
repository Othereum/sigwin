//! Small state files holding account data (message previews, drafts, thread keys, which for a group
//! is its master key), in the vault's format. Older versions kept them as plain JSON next to it.

use std::path::Path;

/// The file's contents. A `legacy` plain file is moved over first, and only removed once its data
/// is safe in `path`. Deleting doesn't wipe the disk, but it ends what backups and sync can copy.
pub fn read(path: &Path, legacy: &Path) -> Option<Vec<u8>> {
    match sigwin_vault::read(path) {
        Ok(data) => {
            remove_legacy(legacy);
            return Some(data);
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => tracing::warn!("read {}: {e}", path.display()),
        Err(_) => {}
    }
    let data = std::fs::read(legacy).ok()?;
    match sigwin_vault::write(path, &data) {
        Ok(()) => remove_legacy(legacy),
        Err(e) => tracing::warn!("seal {}: {e}", legacy.display()),
    }
    Some(data)
}

/// As safe against a crash as the plain files were: replaced in one step, no flush to the disk.
pub fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    sigwin_vault::write_cache(path, data)
}

fn remove_legacy(legacy: &Path) {
    let _ = std::fs::remove_file(legacy);
    let _ = std::fs::remove_file(legacy.with_extension("json.tmp"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_files_move_over_once() {
        sigwin_vault::init([7; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-sealed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, legacy) = (dir.join("state.enc"), dir.join("state.json"));
        let secret = b"{\"snippets\":\"plain words\"}";

        std::fs::write(&legacy, secret).unwrap();
        std::fs::write(legacy.with_extension("json.tmp"), secret).unwrap();
        assert_eq!(read(&path, &legacy).unwrap(), secret);
        assert!(!legacy.exists() && !legacy.with_extension("json.tmp").exists());
        let on_disk = std::fs::read(&path).unwrap();
        assert!(!on_disk.windows(11).any(|w| w == b"plain words"));
        assert_eq!(read(&path, &legacy).unwrap(), secret);

        // what's in the new file wins over a plain one left behind
        write(&path, b"new").unwrap();
        std::fs::write(&legacy, b"old").unwrap();
        assert_eq!(read(&path, &legacy).unwrap(), b"new");
        assert!(!legacy.exists());

        // nothing yet
        std::fs::remove_file(&path).unwrap();
        assert!(read(&path, &legacy).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
