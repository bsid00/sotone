//! Filesystem helpers shared by everything that owns a user file.
//!
//! There is exactly one rule in here: a file the user cares about is never
//! left half-written. Every save goes to a temp file in the *same* directory,
//! is flushed to disk, and is then renamed over the target. A rename within a
//! directory is atomic on both NTFS and POSIX filesystems, so a crash or power
//! loss leaves either the complete old file or the complete new one — never a
//! truncated hybrid.
//!
//! Deliberately dependency-free: the draft store and the markdown
//! save reuse this, and it is small enough that pulling a crate in for it
//! would be more risk than writing it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bumped per staged file so two writes in the same nanosecond, in the same
/// process, cannot collide on a temp name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` so that readers see either the previous contents or
/// the complete new contents, never anything in between.
///
/// Missing parent directories are created. On success the temp file is gone;
/// on failure the target is untouched and the temp file is cleaned up.
///
/// # Errors
/// Returns the underlying [`io::Error`] if the directory cannot be created, the
/// temp file cannot be written or flushed, or the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = stage_temp(path, bytes)?;

    // `fs::rename` replaces an existing destination on Windows as well as on
    // POSIX, so this is a single atomic swap rather than remove-then-move.
    if let Err(err) = fs::rename(&temp, path) {
        // Only ever removes the temp file this call created. No user file is
        // deleted anywhere in this module.
        let _ = fs::remove_file(&temp);
        return Err(err);
    }

    sync_parent_dir(path);
    Ok(())
}

/// Write `bytes` to a fresh temp file beside `path` and flush it to disk,
/// returning the temp path. The target is not touched.
///
/// Split out from [`write_atomic`] so tests can simulate an interruption
/// between "data is on disk" and "rename happened".
pub(crate) fn stage_temp(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = parent_dir(path);
    fs::create_dir_all(&dir)?;

    let temp = dir.join(temp_file_name(path));

    // `create_new` rather than `create`: if the name somehow already exists we
    // want an error, not to stomp on whatever is there.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;

    match file.write_all(bytes).and_then(|()| file.sync_all()) {
        Ok(()) => {
            drop(file);
            Ok(temp)
        }
        Err(err) => {
            drop(file);
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// The directory the temp file must live in: same volume, same directory, so
/// the rename stays atomic. A bare filename means the current directory.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn temp_file_name(path: &Path) -> String {
    let stem = path
        .file_name()
        .map_or_else(|| "sotone".to_owned(), |n| n.to_string_lossy().into_owned());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);

    format!(".{stem}.{}.{seq}.{nanos}.tmp", std::process::id())
}

/// Durability belt-and-braces: on POSIX the rename itself is only guaranteed to
/// survive a crash once the *directory* entry is flushed. Best effort — if the
/// directory cannot be opened there is nothing useful to do about it, and the
/// write itself already succeeded.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) {
    if let Ok(dir) = fs::File::open(parent_dir(path)) {
        let _ = dir.sync_all();
    }
}

/// Windows has no portable way to open a directory handle for flushing, and
/// NTFS metadata journalling covers the rename anyway.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("nested").join("deeper").join("out.txt");

        write_atomic(&target, b"hello").expect("write");

        assert_eq!(fs::read(&target).expect("read"), b"hello");
    }

    #[test]
    fn write_atomic_replaces_existing_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("out.txt");

        write_atomic(&target, b"first").expect("first write");
        write_atomic(&target, b"second").expect("second write");

        assert_eq!(fs::read(&target).expect("read"), b"second");
    }

    #[test]
    fn write_atomic_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("out.txt");

        write_atomic(&target, b"payload").expect("write");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "out.txt")
            .collect();
        assert!(leftovers.is_empty(), "unexpected leftovers: {leftovers:?}");
    }

    #[test]
    fn interrupted_write_leaves_the_original_intact() {
        // Simulates a crash after the data is safely on disk but before the
        // rename: the reader must still see the complete old file.
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("notes.md");
        fs::write(&target, b"original").expect("seed");

        let temp = stage_temp(&target, b"replacement").expect("stage");

        assert_eq!(fs::read(&target).expect("read"), b"original");
        assert_eq!(fs::read(&temp).expect("read temp"), b"replacement");
        assert_ne!(temp, target);
    }

    #[test]
    fn staged_temp_lives_in_the_same_directory_as_the_target() {
        // Cross-directory renames are not atomic, so this is load-bearing.
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("sub").join("notes.md");

        let temp = stage_temp(&target, b"x").expect("stage");

        assert_eq!(temp.parent(), target.parent());
    }

    #[test]
    fn temp_names_do_not_collide_within_a_process() {
        let path = Path::new("some/dir/config.toml");
        let a = temp_file_name(path);
        let b = temp_file_name(path);
        assert_ne!(a, b);
    }
}
