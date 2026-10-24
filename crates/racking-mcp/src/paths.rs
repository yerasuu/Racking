//! The one place where paths named by the model are checked. Every file the
//! tools read or write must sit inside the root directory (`RACK_MCP_ROOT`,
//! else `CLAUDE_PROJECT_DIR`, else the working directory), so a job body that talks an agent into
//! attaching `~/.ssh/id_ed25519` or saving over `~/.bashrc` gets refused.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::args::AttachmentName;

/// Canonical, so `starts_with` checks compare resolved paths.
pub struct Root(PathBuf);

/// A regular file inside the root, resolved through any symlinks.
pub struct FileToSend {
    pub path: PathBuf,
    pub len: u64,
}

/// Where a download may be written: inside the root, not an existing file
/// unless the caller allowed replacing it.
pub struct Destination(PathBuf);

impl Destination {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

pub enum Overwrite {
    Allowed,
    Refused,
}

impl Root {
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::canonicalize(dir).map(Self)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn file_to_send(&self, given: &str) -> Result<FileToSend, String> {
        let path = fs::canonicalize(self.0.join(given)).map_err(|e| format!("cannot open `{given}`: {e}"))?;
        self.check_inside(&path, given)?;
        let meta = fs::metadata(&path).map_err(|e| format!("cannot read `{given}`: {e}"))?;
        if !meta.is_file() {
            return Err(format!("`{given}` is not a regular file"));
        }
        Ok(FileToSend { path, len: meta.len() })
    }

    /// Creates `dir` if missing. `..` is refused outright and every existing
    /// ancestor is resolved before anything is created, so a symlink inside the
    /// root cannot lead the new directories outside it.
    pub fn destination(&self, dir: &str, name: &AttachmentName, overwrite: Overwrite) -> Result<Destination, String> {
        if Path::new(dir).components().any(|c| c == Component::ParentDir) {
            return Err(format!("dir `{dir}` must not contain `..`"));
        }
        let wanted = self.0.join(dir);
        let existing = wanted
            .ancestors()
            .find(|a| a.exists())
            .ok_or_else(|| format!("dir `{dir}` has no existing parent"))?;
        let resolved = fs::canonicalize(existing).map_err(|e| format!("cannot resolve `{dir}`: {e}"))?;
        self.check_inside(&resolved, dir)?;
        fs::create_dir_all(&wanted).map_err(|e| format!("cannot create `{dir}`: {e}"))?;
        let dir_path = fs::canonicalize(&wanted).map_err(|e| format!("cannot resolve `{dir}`: {e}"))?;
        self.check_inside(&dir_path, dir)?;
        let dest = dir_path.join(name.as_str());
        if matches!(overwrite, Overwrite::Refused) && fs::symlink_metadata(&dest).is_ok() {
            return Err(format!(
                "{} already exists; pass overwrite=true to replace it, or choose another dir",
                dest.display()
            ));
        }
        Ok(Destination(dest))
    }

    fn check_inside(&self, resolved: &Path, given: &str) -> Result<(), String> {
        if resolved.starts_with(&self.0) {
            Ok(())
        } else {
            Err(format!(
                "`{given}` is outside {}; only files under that directory can be sent or saved",
                self.0.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "racking-mcp-paths-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("root/sub")).expect("scratch");
            fs::write(dir.join("root/sub/a.txt"), "a").expect("file");
            fs::write(dir.join("secret"), "s").expect("outside file");
            Self(dir)
        }

        fn root(&self) -> Root {
            Root::new(&self.0.join("root")).expect("root")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn name(n: &str) -> AttachmentName {
        AttachmentName::try_from(n.to_owned()).expect("name")
    }

    #[test]
    fn files_to_send_stay_inside() {
        let s = Scratch::new();
        let root = s.root();
        assert_eq!(root.file_to_send("sub/a.txt").map(|f| f.len), Ok(1));
        assert!(root.file_to_send("../secret").is_err());
        assert!(root.file_to_send(&s.0.join("secret").display().to_string()).is_err());
        assert!(root.file_to_send("sub").is_err());
        assert!(root.file_to_send("missing").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_are_refused() {
        let s = Scratch::new();
        std::os::unix::fs::symlink(s.0.join("secret"), s.0.join("root/link")).expect("link");
        std::os::unix::fs::symlink(&s.0, s.0.join("root/out")).expect("dir link");
        let root = s.root();
        assert!(root.file_to_send("link").is_err());
        assert!(root.destination("out/new", &name("x.log"), Overwrite::Refused).is_err());
        assert!(!s.0.join("new").exists());
    }

    #[test]
    fn destinations() {
        let s = Scratch::new();
        let root = s.root();
        let d = root
            .destination("in/j1", &name("x.log"), Overwrite::Refused)
            .expect("new dir");
        assert_eq!(d.path(), root.path().join("in/j1/x.log"));
        assert!(root.path().join("in/j1").is_dir());
        assert!(root.destination("sub", &name("a.txt"), Overwrite::Refused).is_err());
        assert!(root.destination("sub", &name("a.txt"), Overwrite::Allowed).is_ok());
        assert!(root.destination("../x", &name("a.txt"), Overwrite::Allowed).is_err());
        assert!(
            root.destination("sub/../../x", &name("a.txt"), Overwrite::Allowed)
                .is_err()
        );
        let outside = s.0.join("elsewhere").display().to_string();
        assert!(root.destination(&outside, &name("a.txt"), Overwrite::Allowed).is_err());
        assert!(!s.0.join("elsewhere").exists());
        let inside = root.path().join("abs").display().to_string();
        assert!(root.destination(&inside, &name("a.txt"), Overwrite::Refused).is_ok());
    }
}
