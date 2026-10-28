//! Path checks of the policy. Every path is resolved before it is compared: absolute only, no `..`,
//! symlinks of its existing part followed, so a link inside the workdir that points outside it
//! is judged by where it points.

use std::fs;
use std::path::{Component, Path, PathBuf};

use super::glob;

/// How paths compare: Windows file systems ignore case, Unix ones do not.
#[derive(Clone, Copy)]
pub enum Case {
    Sensitive,
    Insensitive,
}

pub const CASE: Case = if cfg!(windows) {
    Case::Insensitive
} else {
    Case::Sensitive
};

/// A path resolved and normalised for comparison: forward slashes, no `\\?\` prefix, lower case where
/// the file system ignores case. Only `resolve` and the policy loader make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved(String);

impl Resolved {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A path kept only to finish validating a policy that has already failed.
    pub(super) fn unchecked(path: &str) -> Self {
        Self(normalize(path, CASE))
    }

    /// Whether this path is `base` or lies under it, compared by whole components.
    #[must_use]
    pub fn within(&self, base: &Resolved) -> bool {
        within(&self.0, &base.0)
    }

    /// Whether `pattern` (a resolved path, possibly with wildcards) covers this path: a plain path
    /// covers itself and everything below it; a pattern matches one path component per component.
    #[must_use]
    pub fn covered_by(&self, pattern: &Resolved) -> bool {
        if glob::is_pattern(&pattern.0) {
            glob::matches_path(&pattern.0, &self.0)
        } else {
            within(&self.0, &pattern.0)
        }
    }
}

fn within(path: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    path == base || (path.len() > base.len() && path.starts_with(base) && path.as_bytes()[base.len()] == b'/')
        // The root itself ends up as "" once trimmed: everything is under it.
        || base.is_empty()
}

/// The comparable form of a path string.
#[must_use]
pub fn normalize(path: &str, case: Case) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path = path.replace('\\', "/");
    match case {
        Case::Sensitive => path,
        Case::Insensitive => path.to_lowercase(),
    }
}

/// Resolves an absolute path for a check. The part that exists is canonicalised (symlinks
/// followed); the rest, which cannot be a link yet, is appended as written.
///
/// # Errors
///
/// A relative path, a `..` component, or a dangling symlink, since following one could create a
/// file wherever it points.
pub fn resolve(path: &Path) -> Result<Resolved, String> {
    Ok(Resolved(normalize(&real(path)?.to_string_lossy(), CASE)))
}

fn real(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("`{}` is not an absolute path", path.display()));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("`{}` contains `..`", path.display()));
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = fs::canonicalize(&existing) {
            return Ok(rest.iter().rev().fold(real, |mut out, name| {
                out.push(name);
                out
            }));
        }
        if fs::symlink_metadata(&existing).is_ok() {
            return Err(format!(
                "`{}` is a link that cannot be followed (dangling or unreadable)",
                existing.display()
            ));
        }
        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
            return Err(format!("`{}` has no existing parent", path.display()));
        };
        rest.push(name);
        if !existing.pop() {
            return Err(format!("`{}` has no existing parent", path.display()));
        }
    }
}

/// Resolves a policy path entry that may hold wildcards: the plain part before the first wildcard
/// component is resolved like any path, the rest is kept as a pattern.
///
/// # Errors
///
/// As `resolve`, for the plain part.
pub fn resolve_pattern(entry: &str) -> Result<Resolved, String> {
    let path = Path::new(entry);
    if !path.is_absolute() {
        return Err(format!("`{entry}` is not an absolute path"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("`{entry}` contains `..`"));
    }
    let mut plain = PathBuf::new();
    let mut tail = Vec::new();
    for c in path.components() {
        let text = c.as_os_str().to_string_lossy();
        if !tail.is_empty() || glob::is_pattern(&text) {
            tail.push(normalize(&text, CASE));
        } else {
            plain.push(c);
        }
    }
    let base = resolve(&plain)?;
    if tail.is_empty() {
        return Ok(base);
    }
    let sep = if base.0.ends_with('/') { "" } else { "/" };
    Ok(Resolved(format!("{}{sep}{}", base.0, tail.join("/"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rack-paths-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir");
            Self(fs::canonicalize(&dir).expect("canonical"))
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn relative_and_dotdot_are_refused() {
        assert!(resolve(Path::new("work/a.txt")).is_err());
        let dir = Dir::new("dotdot");
        let err = resolve(&dir.0.join("../etc/passwd")).expect_err("..");
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn missing_tail_is_appended_to_the_real_parent() {
        let dir = Dir::new("tail");
        let got = resolve(&dir.0.join("new/file.txt")).expect("resolve");
        let base = resolve(&dir.0).expect("base");
        assert!(got.within(&base));
        assert!(got.as_str().ends_with("/new/file.txt"));
    }

    #[test]
    fn components_not_prefixes() {
        let base = Resolved("/home/x/work".to_owned());
        assert!(Resolved("/home/x/work".to_owned()).within(&base));
        assert!(Resolved("/home/x/work/a".to_owned()).within(&base));
        assert!(!Resolved("/home/x/workshop".to_owned()).within(&base));
        assert!(!Resolved("/home/x".to_owned()).within(&base));
        assert!(Resolved("/anything".to_owned()).within(&Resolved("/".to_owned())));
    }

    #[test]
    fn windows_paths_compare_without_case_and_prefix() {
        let a = normalize(r"\\?\C:\Users\X\Work\a.txt", Case::Insensitive);
        let b = normalize(r"c:\users\x\work", Case::Insensitive);
        assert_eq!(a, "c:/users/x/work/a.txt");
        assert!(within(&a, &b));
        assert!(!within(&normalize(r"C:\Users\X\Workshop", Case::Insensitive), &b));
        assert_ne!(
            normalize("/home/X", Case::Sensitive),
            normalize("/home/x", Case::Sensitive)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_judged_by_their_target() {
        let dir = Dir::new("link");
        let work = dir.0.join("work");
        let outside = dir.0.join("outside");
        fs::create_dir_all(&work).expect("work");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, work.join("escape")).expect("link");
        std::os::unix::fs::symlink(dir.0.join("nowhere"), work.join("dangling")).expect("link");
        let base = resolve(&work).expect("work");
        let escaped = resolve(&work.join("escape/secret.txt")).expect("resolve");
        assert!(!escaped.within(&base), "{escaped:?}");
        assert!(resolve(&work.join("dangling")).is_err());
        assert!(resolve(&work.join("dangling/x")).is_err());
    }

    #[test]
    fn patterns_resolve_their_plain_part() {
        let dir = Dir::new("pattern");
        fs::create_dir_all(dir.0.join("logs")).expect("logs");
        let pat = resolve_pattern(&format!("{}/logs/*.log", dir.0.display())).expect("pattern");
        let hit = resolve(&dir.0.join("logs/a.log")).expect("hit");
        let deeper = resolve(&dir.0.join("logs/x/a.log")).expect("deeper");
        assert!(hit.covered_by(&pat));
        assert!(!deeper.covered_by(&pat));
        assert!(resolve_pattern("logs/*.log").is_err());
    }
}
