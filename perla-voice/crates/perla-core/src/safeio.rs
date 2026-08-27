//! Guarded reads and writes for the daemon's predictable file paths.
//!
//! Perla's config, state, and log paths are all derivable by anything running
//! as the same user, and a long-lived daemon that touches them naively is easy
//! to wedge or redirect:
//!
//! - `read_to_string` on a planted FIFO blocks the caller forever, and on a
//!   planted huge file it reads until memory runs out.
//! - `OpenOptions::create().truncate().write()` follows a planted symlink, so
//!   a config write carrying an API key can be steered into some other file
//!   the user owns — and a crash mid-write leaves the real config truncated.
//!
//! Everything here is same-user hardening. It does not defend against a user
//! attacking their own account with root; it removes the cheap tricks that a
//! rogue process in the same session could otherwise pull on a daemon that is
//! holding an API key.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Ceiling for any of the daemon's own files. Config and state are a few KiB;
/// the log is trimmed well below this. Anything larger is not ours.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Read a path we expect to own, or `Ok(None)` if it is simply not there.
///
/// Refuses symlinks, refuses anything that is not a regular file (so a FIFO
/// cannot park the daemon), and refuses to read past `max_bytes`.
pub fn read_text(path: &Path, max_bytes: u64) -> Result<Option<String>> {
    let mut file = match open_regular_nofollow(path)? {
        Some(file) => file,
        None => return Ok(None),
    };

    let len = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?
        .len();
    if len > max_bytes {
        bail!(
            "{} is {len} bytes, over the {max_bytes} byte limit",
            path.display()
        );
    }

    // Bound the read itself too: the size above is a snapshot, and a writer
    // could grow the file between the stat and the read.
    let mut text = String::new();
    let read = Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    if read as u64 > max_bytes {
        bail!("{} grew past the {max_bytes} byte limit", path.display());
    }
    Ok(Some(text))
}

/// [`read_text`] with the default ceiling.
pub fn read_text_capped(path: &Path) -> Result<Option<String>> {
    read_text(path, MAX_FILE_BYTES)
}

/// Best-effort variant for callers that would rather have nothing than an
/// error — a missing, hostile, or unreadable file all collapse to `None`.
pub fn read_text_opt(path: &Path) -> Option<String> {
    read_text_capped(path).ok().flatten()
}

/// Open for reading without following a final symlink, and only if the result
/// is a regular file. `Ok(None)` means "not there".
fn open_regular_nofollow(path: &Path) -> Result<Option<File>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW fails on a symlinked final component. O_NONBLOCK keeps
        // the open itself from hanging on a FIFO with no writer; the regular
        // file check below rejects it a moment later anyway.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) => {
            return match err.kind() {
                std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(err).with_context(|| format!("opening {}", path.display())),
            }
        }
    };

    let meta = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(Some(file))
}

/// Replace `path` with `body`, atomically, never through a symlink, never
/// group- or world-readable.
///
/// Writes a freshly created sibling with `O_EXCL` (so an existing symlink at
/// the temporary name cannot capture the write either), fsyncs it, renames it
/// over the target — `rename` replaces a symlink rather than following it —
/// and fsyncs the directory so the rename itself survives a power cut.
pub fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let (mut file, tmp_path) = create_exclusive_temp(&parent, path)?;

    let result = (|| -> Result<()> {
        file.write_all(body)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp_path.display()))?;
        Ok(())
    })();
    drop(file);

    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err).with_context(|| format!("replacing {}", path.display()));
    }

    // Without this the rename can be lost while the file contents survive,
    // which is how you end up with a config that exists but is empty.
    if let Ok(dir) = File::open(&parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Create `.<name>.<pid>.<n>.tmp` next to the target, failing rather than
/// reusing anything that already exists.
fn create_exclusive_temp(parent: &Path, target: &Path) -> Result<(File, PathBuf)> {
    let stem = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "perla".into());
    let pid = std::process::id();

    let mut last_err = None;
    for attempt in 0..32u32 {
        let candidate = parent.join(format!(".{stem}.{pid}.{attempt}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(err);
                continue;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("creating a temporary file in {}", parent.display()))
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free temporary name")
    }))
    .with_context(|| format!("creating a temporary file in {}", parent.display()))
}

/// Create a directory only its owner can enter.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("perla-safeio-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_a_regular_file_and_reports_a_missing_one() {
        let dir = scratch("read");
        let path = dir.join("config.toml");
        assert!(read_text_capped(&path).unwrap().is_none());
        std::fs::write(&path, "model = \"x\"\n").unwrap();
        assert_eq!(
            read_text_capped(&path).unwrap().as_deref(),
            Some("model = \"x\"\n")
        );
    }

    #[test]
    fn refuses_a_file_over_the_limit() {
        let dir = scratch("big");
        let path = dir.join("config.toml");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        assert!(read_text(&path, 8).is_err());
        assert!(read_text_opt(&path).is_some(), "the default cap is generous");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_read_through_a_symlink() {
        let dir = scratch("symlink-read");
        let secret = dir.join("secret");
        std::fs::write(&secret, "private\n").unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(read_text_capped(&link).is_err());
        assert!(read_text_opt(&link).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_read_a_fifo_instead_of_blocking() {
        let dir = scratch("fifo");
        let path = dir.join("config.toml");
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // If the guard regressed this test would hang rather than fail, which
        // is exactly the daemon behaviour it exists to prevent.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(read_text_capped(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_does_not_capture_the_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("symlink-write");
        let victim = dir.join("victim");
        std::fs::write(&victim, "untouched\n").unwrap();
        let path = dir.join("config.toml");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_private(&path, b"api_key = \"sk-secret\"\n").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "api_key = \"sk-secret\"\n"
        );
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn writing_replaces_and_leaves_no_temporary_behind() {
        let dir = scratch("replace");
        let path = dir.join("config.toml");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files were left behind");
    }
}
