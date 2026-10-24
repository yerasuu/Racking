//! Swapping the running executable for a checked new one. Both platform paths
//! are plain functions over paths, compiled everywhere, so the Windows
//! rename-and-restore steps are tested on Linux too.

use std::fs;
use std::io;
use std::path::Path;

/// Unix: `rename` over the executable is atomic, and the running process keeps
/// its already mapped image, so there is never a moment without a `rack`.
pub fn replace_unix(new: &Path, exe: &Path) -> Result<(), String> {
    fs::rename(new, exe).map_err(|e| format!("cannot move the new binary over {}: {e}", exe.display()))
}

/// Windows refuses to overwrite or delete a running executable but lets it be
/// renamed, so the old one moves aside to `old` and the new one takes its
/// name. If that last move fails, the old one is moved back. `old` stays
/// behind (it is still running) and is removed by the next update.
pub fn replace_windows(new: &Path, exe: &Path, old: &Path) -> Result<(), String> {
    match fs::remove_file(old) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot remove the stale {} (is an older rack still running?): {e}",
                old.display()
            ));
        }
    }
    fs::rename(exe, old).map_err(|e| format!("cannot move {} aside: {e}", exe.display()))?;
    if let Err(e) = fs::rename(new, exe) {
        return match fs::rename(old, exe) {
            Ok(()) => Err(format!("cannot move the new binary into {}: {e}", exe.display())),
            Err(restore) => Err(format!(
                "cannot move the new binary into {} ({e}) nor restore the old one ({restore}); \
                 the old rack is at {}",
                exe.display(),
                old.display()
            )),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static DIRS: AtomicUsize = AtomicUsize::new(0);
            let n = DIRS.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("rack-replace-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_else(|e| format!("<{e}>"))
    }

    #[test]
    fn unix_renames_over() {
        let dir = TempDir::new();
        let (new, exe) = (dir.0.join(".rack-update"), dir.0.join("rack"));
        fs::write(&exe, "old").expect("exe");
        fs::write(&new, "new").expect("new");
        assert_eq!(replace_unix(&new, &exe), Ok(()));
        assert_eq!(read(&exe), "new");
        assert!(!new.exists());

        let missing = dir.0.join("missing");
        assert!(replace_unix(&missing, &exe).is_err());
        assert_eq!(read(&exe), "new", "a failed move leaves the binary");
    }

    #[test]
    fn windows_moves_aside_and_removes_stale_old() {
        let dir = TempDir::new();
        let (new, exe, old) = (dir.0.join("new.exe"), dir.0.join("rack.exe"), dir.0.join("rack.old"));
        fs::write(&exe, "v1").expect("exe");
        fs::write(&new, "v2").expect("new");
        fs::write(&old, "v0").expect("stale old");
        assert_eq!(replace_windows(&new, &exe, &old), Ok(()));
        assert_eq!((read(&exe).as_str(), read(&old).as_str()), ("v2", "v1"));
        assert!(!new.exists());
    }

    #[test]
    fn windows_restores_when_the_new_binary_cannot_move_in() {
        let dir = TempDir::new();
        let (exe, old) = (dir.0.join("rack.exe"), dir.0.join("rack.old"));
        fs::write(&exe, "v1").expect("exe");
        let err = replace_windows(&dir.0.join("gone.exe"), &exe, &old);
        assert!(err.is_err());
        assert_eq!(read(&exe), "v1", "the old binary is back in place");
        assert!(!old.exists());
    }

    #[test]
    fn windows_stops_before_touching_the_exe_when_old_cannot_go() {
        let dir = TempDir::new();
        let (new, exe, old) = (dir.0.join("new.exe"), dir.0.join("rack.exe"), dir.0.join("rack.old"));
        fs::write(&exe, "v1").expect("exe");
        fs::write(&new, "v2").expect("new");
        // A non-empty directory stands for a stale rack.old that cannot be removed.
        fs::create_dir_all(old.join("busy")).expect("blocker");
        assert!(replace_windows(&new, &exe, &old).is_err());
        assert_eq!(read(&exe), "v1");
    }
}
