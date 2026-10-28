//! Unpacks a ustar archive received from another agent, which is untrusted
//! data. The archive is read twice: a first pass checks every header, name,
//! size and the place each entry would land, and only an archive that passes
//! whole is written in the second pass, so a hostile one writes nothing.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::{BLOCK, MAGIC, TYPE_DIR, TYPE_FILE, VERSION, bad_path, checksum, field, padding};

/// Whether an entry may replace a file that already exists in the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    Refuse,
    Allow,
}

/// Caps on what one archive may unpack.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Total bytes of the files, as their headers declare them.
    pub max_bytes: u64,
    /// Entries, files and directories together.
    pub max_entries: u64,
    pub overwrite: Overwrite,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024 * 1024,
            max_entries: 100_000,
            overwrite: Overwrite::Refuse,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

struct Entry {
    /// Checked by `bad_path`, without a trailing slash.
    path: String,
    kind: Kind,
    exec: bool,
    size: u64,
}

/// Unpacks `archive` into `dest`, which must exist. Refuses, before writing
/// anything: pax and GNU extensions, links, devices and FIFOs, names that are
/// absolute, hold `..`, a backslash, a colon or a NUL, duplicate entries, a
/// size or entry count over `limits`, a truncated archive, and any entry that
/// would land on an existing file (unless `Overwrite::Allow`), on a symlink or
/// outside `dest`. Files get mode 0o644, or 0o755 when the archive marks them
/// executable; setuid, setgid and sticky bits are dropped.
pub fn extract(archive: &Path, dest: &Path, limits: &Limits) -> Result<Extracted, String> {
    let root = dest
        .canonicalize()
        .map_err(|e| format!("cannot open {}: {e}", dest.display()))?;
    let open = || File::open(archive).map_err(|e| format!("cannot open {}: {e}", archive.display()));
    walk(&mut io::BufReader::new(open()?), limits, |entry, data| {
        check_target(&root, entry, limits.overwrite)?;
        io::copy(data, &mut io::sink())
            .map(drop)
            .map_err(|e| format!("cannot read the archive: {e}"))
    })?;
    walk(&mut io::BufReader::new(open()?), limits, |entry, data| {
        write_entry(&root, entry, data, limits.overwrite)
    })
}

/// Reads every entry, checks it, and hands it with its data to `visit`.
fn walk(
    r: &mut dyn Read,
    limits: &Limits,
    mut visit: impl FnMut(&Entry, &mut dyn Read) -> Result<(), String>,
) -> Result<Extracted, String> {
    let mut layout = Layout::default();
    let mut done = Extracted::default();
    loop {
        let mut block = [0u8; BLOCK];
        read_full(r, &mut block)?;
        if block.iter().all(|b| *b == 0) {
            return Ok(done);
        }
        let entry = parse_header(&block)?;
        layout.add(&entry)?;
        if done.files + done.dirs >= limits.max_entries {
            return Err(format!(
                "the archive has more than {} entries (--max-files)",
                limits.max_entries
            ));
        }
        done.bytes = done.bytes.saturating_add(entry.size);
        if done.bytes > limits.max_bytes {
            return Err(format!(
                "the archive unpacks to more than {} bytes (--max-bytes)",
                limits.max_bytes
            ));
        }
        match entry.kind {
            Kind::File => done.files += 1,
            Kind::Dir => done.dirs += 1,
        }
        let mut data = (&mut *r).take(entry.size);
        visit(&entry, &mut data)?;
        // Whatever the visitor left unread is skipped; data shorter than declared is a cut archive.
        io::copy(&mut data, &mut io::sink()).map_err(|e| format!("cannot read the archive: {e}"))?;
        if data.limit() != 0 {
            return Err(truncated());
        }
        let mut pad = [0u8; BLOCK];
        let pad_len = usize::try_from(padding(entry.size)).unwrap_or(0);
        read_full(r, &mut pad[..pad_len])?;
    }
}

/// The paths an archive has named so far, so that two entries which would
/// collide on disk are refused in the checking pass, before anything is written.
#[derive(Default)]
struct Layout {
    /// Every path, lowercased: case-insensitive file systems (Windows, macOS)
    /// would make `A.txt` and `a.txt` one file.
    seen: HashSet<String>,
    files: HashSet<String>,
    /// Directories, named or implied by a deeper entry.
    dirs: HashSet<String>,
}

impl Layout {
    fn add(&mut self, entry: &Entry) -> Result<(), String> {
        let folded = entry.path.to_lowercase();
        if !self.seen.insert(folded.clone()) {
            return Err(format!(
                "`{}` appears twice in the archive (names that differ only by case count as one)",
                entry.path
            ));
        }
        for (i, _) in folded.match_indices('/') {
            let parent = &folded[..i];
            if self.files.contains(parent) {
                return Err(format!("`{}` would go inside the file `{parent}`", entry.path));
            }
            self.dirs.insert(parent.to_owned());
        }
        match entry.kind {
            Kind::File if self.dirs.contains(&folded) => {
                Err(format!("`{}` is a file and a directory in the archive", entry.path))
            }
            Kind::File => {
                self.files.insert(folded);
                Ok(())
            }
            Kind::Dir => {
                self.dirs.insert(folded);
                Ok(())
            }
        }
    }
}

fn truncated() -> String {
    "the archive is truncated: it ends inside an entry or before its end marker".to_owned()
}

/// Fills `buf` or fails: the archive never ends inside a header, data or padding.
fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> Result<(), String> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => truncated(),
        _ => format!("cannot read the archive: {e}"),
    })
}

fn parse_header(h: &[u8; BLOCK]) -> Result<Entry, String> {
    let magic = &h[field::MAGIC.0..field::MAGIC.0 + field::MAGIC.1];
    let version = &h[field::VERSION.0..field::VERSION.0 + field::VERSION.1];
    if magic == b"ustar " {
        return Err("GNU tar format is not supported: pack it as POSIX ustar (`tar --format=ustar`)".to_owned());
    }
    if magic != MAGIC || version != VERSION {
        return Err("not a ustar archive (only POSIX ustar is supported)".to_owned());
    }
    let stored = number(h, field::CHKSUM, "checksum")?;
    if stored != checksum(h) {
        return Err("a header checksum is wrong: the archive is corrupt".to_owned());
    }
    let kind = match h[field::TYPEFLAG] {
        TYPE_FILE | b'\0' | b'7' => Kind::File,
        TYPE_DIR => Kind::Dir,
        b'1' => return Err(refused(h, "hard link")),
        b'2' => return Err(refused(h, "symlink")),
        b'3' | b'4' => return Err(refused(h, "device node")),
        b'6' => return Err(refused(h, "FIFO")),
        b'x' | b'g' => return Err("pax extended headers are not supported: pack it as plain ustar".to_owned()),
        b'L' | b'K' => return Err("GNU long names are not supported: pack it as plain ustar".to_owned()),
        other => return Err(format!("entry type `{}` is not supported", other.escape_ascii())),
    };
    let name = text(h, field::NAME)?;
    let prefix = text(h, field::PREFIX)?;
    let full = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    };
    let path = match kind {
        Kind::Dir => full.strip_suffix('/').unwrap_or(&full),
        Kind::File => full.as_str(),
    };
    if let Some(why) = bad_path(path) {
        return Err(format!("refusing `{}`: {why}", full.escape_debug()));
    }
    let size = number(h, field::SIZE, "size")?;
    if kind == Kind::Dir && size != 0 {
        return Err(format!("directory `{path}` declares {size} bytes of data"));
    }
    let mode = number(h, field::MODE, "mode")?;
    Ok(Entry {
        path: path.to_owned(),
        kind,
        exec: mode & 0o111 != 0,
        size,
    })
}

fn refused(h: &[u8; BLOCK], what: &str) -> String {
    let name = text(h, field::NAME).unwrap_or("?");
    format!(
        "refusing `{}`: a {what}; archives may hold only files and directories",
        name.escape_debug()
    )
}

/// A NUL-terminated text field; any byte after the first NUL other than NUL
/// means a name with a NUL in it, which is refused rather than cut.
fn text(h: &[u8; BLOCK], (at, len): (usize, usize)) -> Result<&str, String> {
    let raw = &h[at..at + len];
    let end = raw.iter().position(|b| *b == 0).unwrap_or(len);
    if raw[end..].iter().any(|b| *b != 0) {
        return Err("a name holds a NUL byte".to_owned());
    }
    std::str::from_utf8(&raw[..end]).map_err(|_| "a name is not UTF-8".to_owned())
}

/// An octal number field: optional leading spaces, digits, then NUL or space.
fn number(h: &[u8; BLOCK], (at, len): (usize, usize), what: &str) -> Result<u64, String> {
    let raw = &h[at..at + len];
    if raw[0] & 0x80 != 0 {
        return Err(format!("the {what} uses GNU base-256 numbers, which are not supported"));
    }
    let digits = raw
        .iter()
        .skip_while(|b| **b == b' ')
        .take_while(|b| **b != 0 && **b != b' ');
    let mut value = 0u64;
    let mut any = false;
    for b in digits {
        if !(b'0'..=b'7').contains(b) {
            return Err(format!("the {what} field is not an octal number"));
        }
        value = value
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| format!("the {what} field overflows"))?;
        any = true;
    }
    if any {
        Ok(value)
    } else {
        Err(format!("the {what} field is empty"))
    }
}

/// Where `entry` lands, checked without creating anything: every parent that
/// exists must be a real directory (never a symlink), and the entry itself
/// must not exist unless it is a directory meeting a directory, or a file the
/// caller allowed to replace.
fn check_target(root: &Path, entry: &Entry, overwrite: Overwrite) -> Result<(), String> {
    let mut at = root.to_path_buf();
    let parts: Vec<&str> = entry.path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let last = i + 1 == parts.len();
        let Ok(meta) = fs::symlink_metadata(&at) else {
            return Ok(());
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(format!("refusing `{}`: {} is a symlink", entry.path, at.display()));
        }
        match (last, entry.kind) {
            (false, _) | (true, Kind::Dir) if kind.is_dir() => {}
            (true, Kind::File) if kind.is_file() && overwrite == Overwrite::Allow => {}
            (true, Kind::File) if kind.is_file() => {
                return Err(format!(
                    "{} exists; pass --overwrite to replace files, or extract elsewhere",
                    at.display()
                ));
            }
            _ => return Err(format!("refusing `{}`: {} is in the way", entry.path, at.display())),
        }
    }
    Ok(())
}

fn write_entry(root: &Path, entry: &Entry, data: &mut dyn Read, overwrite: Overwrite) -> Result<(), String> {
    let (parents, last) = match entry.path.rsplit_once('/') {
        Some((p, l)) => (Some(p), l),
        None => (None, entry.path.as_str()),
    };
    let dir = make_dirs(root, parents)?;
    let target = dir.join(last);
    match entry.kind {
        Kind::Dir => make_dirs(&dir, Some(last)).map(drop),
        Kind::File => write_file(&target, entry, data, overwrite),
    }
}

/// Creates `parts` under `base` one component at a time, never through a
/// symlink, and checks the result is still inside `base`.
fn make_dirs(base: &Path, parts: Option<&str>) -> Result<PathBuf, String> {
    let mut at = base.to_path_buf();
    for part in parts.into_iter().flat_map(|p| p.split('/')) {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Err(format!("{} is in the way of a directory", at.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&at).map_err(|e| format!("cannot create {}: {e}", at.display()))?;
                set_mode(&at, 0o755)?;
            }
            Err(e) => return Err(format!("cannot read {}: {e}", at.display())),
        }
    }
    let real = at
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", at.display()))?;
    // `base` is canonical already (the destination, or a directory this function returned).
    if real.starts_with(base) {
        Ok(real)
    } else {
        Err(format!("{} would land outside {}", at.display(), base.display()))
    }
}

fn write_file(target: &Path, entry: &Entry, data: &mut dyn Read, overwrite: Overwrite) -> Result<(), String> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_file() && overwrite == Overwrite::Allow => {
            // Removed, not truncated, so a hard link to a file elsewhere is not written through.
            fs::remove_file(target).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
        }
        Ok(_) => return Err(format!("{} exists; refusing to replace it", target.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read {}: {e}", target.display())),
    }
    // `create_new` refuses any existing path, a symlink planted since the check included.
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    let written = io::copy(data, &mut out).and_then(|n| out.flush().map(|()| n));
    match written {
        Ok(n) if n == entry.size => set_mode(target, if entry.exec { 0o755 } else { 0o644 }),
        Ok(_) | Err(_) => {
            drop(out);
            let _ = fs::remove_file(target);
            Err(match written {
                Err(e) => format!("cannot write {}: {e}", target.display()),
                Ok(_) => truncated(),
            })
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set the mode of {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::pack_dir;
    use super::super::write::{header, seal};
    use super::*;

    /// A scratch directory with `out/` (the destination) and room for an archive, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("rack-tar-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("out")).expect("scratch");
            Self(dir)
        }

        fn out(&self) -> PathBuf {
            self.0.join("out")
        }

        fn extract(&self, archive: &[u8], limits: &Limits) -> Result<Extracted, String> {
            let path = self.0.join("in.tar");
            fs::write(&path, archive).expect("archive");
            extract(&path, &self.out(), limits)
        }

        /// Asserts the archive is refused with a message containing `why`, and nothing was written.
        fn refuses(&self, archive: &[u8], why: &str) {
            let err = self.extract(archive, &Limits::default()).err().unwrap_or_default();
            assert!(err.contains(why), "expected `{why}`, got `{err}`");
            let left = fs::read_dir(self.out()).map(Iterator::count).unwrap_or(0);
            assert_eq!(left, 0, "nothing may be written for a refused archive ({why})");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Hand-built archives, including ones no honest writer makes.
    #[derive(Default)]
    struct Tar(Vec<u8>);

    impl Tar {
        fn entry(mut self, h: [u8; BLOCK], data: &[u8]) -> Self {
            self.0.extend_from_slice(&h);
            self.0.extend_from_slice(data);
            self.0.resize(self.0.len() + padding(data.len() as u64) as usize, 0);
            self
        }

        fn file(self, name: &str, data: &[u8]) -> Self {
            self.entry(header(name, b'0', 0o644, data.len() as u64), data)
        }

        fn kind(self, name: &str, kind: u8) -> Self {
            self.entry(header(name, kind, 0o644, 0), &[])
        }

        fn end(mut self) -> Vec<u8> {
            self.0.extend_from_slice(&[0u8; 2 * BLOCK]);
            self.0
        }
    }

    /// A header changed after it was built, with a fresh checksum.
    fn patched(name: &str, size: u64, patch: impl FnOnce(&mut [u8; BLOCK])) -> [u8; BLOCK] {
        let mut h = header(name, b'0', 0o644, size);
        patch(&mut h);
        seal(&mut h);
        h
    }

    #[test]
    fn plain_files_and_directories_unpack() {
        let s = Scratch::new();
        let tar = Tar::default()
            .kind("pkg/", b'5')
            .file("pkg/a.txt", b"alpha")
            .file("pkg/sub/b.txt", b"beta")
            .end();
        let got = s.extract(&tar, &Limits::default()).expect("extract");
        assert_eq!(
            got,
            Extracted {
                files: 2,
                dirs: 1,
                bytes: 9
            }
        );
        assert_eq!(fs::read(s.out().join("pkg/a.txt")).ok().as_deref(), Some(&b"alpha"[..]));
        assert_eq!(
            fs::read(s.out().join("pkg/sub/b.txt")).ok().as_deref(),
            Some(&b"beta"[..])
        );
    }

    #[test]
    fn path_traversal_and_absolute_paths() {
        let s = Scratch::new();
        for name in [
            "../evil",
            "pkg/../../evil",
            "/tmp/evil",
            "..\\..\\evil",
            "pkg\\..\\evil",
            "C:evil",
            "./x",
        ] {
            s.refuses(&Tar::default().file(name, b"x").end(), "refusing");
        }
        let prefixed = patched("evil", 1, |h| h[345..347].copy_from_slice(b".."));
        s.refuses(&Tar::default().entry(prefixed, b"x").end(), "`.` or `..`");
        assert!(!s.0.join("evil").exists());
    }

    #[test]
    fn links_devices_and_extensions_are_refused() {
        let s = Scratch::new();
        s.refuses(&Tar::default().kind("link", b'2').file("link/x", b"x").end(), "symlink");
        s.refuses(&Tar::default().file("a", b"x").kind("b", b'1').end(), "hard link");
        s.refuses(&Tar::default().kind("tty", b'3').end(), "device node");
        s.refuses(&Tar::default().kind("disk", b'4').end(), "device node");
        s.refuses(&Tar::default().kind("pipe", b'6').end(), "FIFO");
        s.refuses(&Tar::default().kind("PaxHeader", b'x').end(), "pax");
        s.refuses(&Tar::default().kind("LongLink", b'L').end(), "GNU long names");
        let gnu = patched("a", 1, |h| h[257..265].copy_from_slice(b"ustar  \0"));
        s.refuses(&Tar::default().entry(gnu, b"x").end(), "GNU tar format");
        let base256 = patched("a", 1, |h| h[124] = 0x80);
        s.refuses(&Tar::default().entry(base256, b"x").end(), "base-256");
        let mut zip = vec![b'P', b'K', 3, 4];
        zip.resize(2048, 0);
        s.refuses(&zip, "not a ustar archive");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_symlink_is_never_followed() {
        let s = Scratch::new();
        let outside = s.0.join("outside");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, s.out().join("link")).expect("symlink");
        let err = s
            .extract(&Tar::default().file("link/x", b"x").end(), &Limits::default())
            .err()
            .unwrap_or_default();
        assert!(err.contains("symlink"), "{err}");
        let err = s
            .extract(&Tar::default().file("link", b"x").end(), &Limits::default())
            .err()
            .unwrap_or_default();
        assert!(err.contains("symlink"), "a file never replaces a symlink: {err}");
        assert_eq!(fs::read_dir(&outside).map(Iterator::count).ok(), Some(0));
    }

    #[test]
    fn duplicates_and_collisions() {
        let s = Scratch::new();
        s.refuses(&Tar::default().file("a.txt", b"1").file("a.txt", b"2").end(), "twice");
        s.refuses(
            &Tar::default().file("A.txt", b"1").file("a.txt", b"2").end(),
            "differ only by case",
        );
        s.refuses(
            &Tar::default().file("a", b"1").file("a/b", b"2").end(),
            "inside the file",
        );
        s.refuses(
            &Tar::default().file("a/b", b"1").file("a", b"2").end(),
            "file and a directory",
        );
    }

    #[test]
    fn short_and_truncated_archives() {
        let s = Scratch::new();
        let huge = Tar::default()
            .entry(header("big.bin", b'0', 0o644, 1 << 30), b"only this")
            .0;
        s.refuses(&huge, "truncated");
        let whole = Tar::default().file("a.txt", b"hello").end();
        s.refuses(&whole[..300], "truncated");
        s.refuses(&whole[..BLOCK + 3], "truncated");
        s.refuses(&whole[..2 * BLOCK], "truncated");
        s.refuses(&[], "truncated");
    }

    #[test]
    fn bad_headers() {
        let s = Scratch::new();
        let mut corrupt = header("a.txt", b'0', 0o644, 1);
        corrupt[0] = b'b';
        s.refuses(&Tar::default().entry(corrupt, b"x").end(), "checksum");
        let nul = patched("ab", 1, |h| h[3] = b'c');
        s.refuses(&Tar::default().entry(nul, b"x").end(), "NUL");
        let not_octal = patched("a", 1, |h| h[124..136].copy_from_slice(b"0000000009x\0"));
        s.refuses(&Tar::default().entry(not_octal, b"x").end(), "octal");
        let dir_with_data = Tar::default().entry(header("d/", b'5', 0o755, 1), b"x").end();
        s.refuses(&dir_with_data, "declares");
    }

    #[test]
    fn caps_on_size_and_count() {
        let s = Scratch::new();
        let tar = Tar::default().file("a", b"12345").file("b", b"123456").end();
        let small = Limits {
            max_bytes: 10,
            ..Limits::default()
        };
        assert!(s.extract(&tar, &small).is_err_and(|e| e.contains("--max-bytes")));
        let few = Limits {
            max_entries: 1,
            ..Limits::default()
        };
        assert!(s.extract(&tar, &few).is_err_and(|e| e.contains("--max-files")));
        assert_eq!(fs::read_dir(s.out()).map(Iterator::count).ok(), Some(0));
        assert!(s.extract(&tar, &Limits::default()).is_ok());
    }

    #[test]
    fn existing_files_are_kept_unless_overwrite() {
        let s = Scratch::new();
        fs::write(s.out().join("a.txt"), b"mine").expect("existing");
        let tar = Tar::default().file("new.txt", b"n").file("a.txt", b"theirs").end();
        let err = s.extract(&tar, &Limits::default()).err().unwrap_or_default();
        assert!(err.contains("--overwrite"), "{err}");
        assert!(
            !s.out().join("new.txt").exists(),
            "the check pass runs before any write"
        );
        assert_eq!(fs::read(s.out().join("a.txt")).ok().as_deref(), Some(&b"mine"[..]));
        let allow = Limits {
            overwrite: Overwrite::Allow,
            ..Limits::default()
        };
        s.extract(&tar, &allow).expect("overwrite");
        assert_eq!(fs::read(s.out().join("a.txt")).ok().as_deref(), Some(&b"theirs"[..]));
        fs::create_dir(s.out().join("d")).expect("dir");
        let err = s
            .extract(&Tar::default().file("d", b"x").end(), &allow)
            .err()
            .unwrap_or_default();
        assert!(err.contains("in the way"), "a file never replaces a directory: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_masked() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        let tar = Tar::default()
            .entry(header("suid", b'0', 0o4777, 1), b"x")
            .entry(header("open", b'0', 0o666, 1), b"y")
            .entry(header("dir/", b'5', 0o7777, 0), &[])
            .end();
        s.extract(&tar, &Limits::default()).expect("extract");
        let mode = |p: &str| {
            fs::metadata(s.out().join(p))
                .map(|m| m.permissions().mode() & 0o7777)
                .ok()
        };
        assert_eq!(
            (mode("suid"), mode("open"), mode("dir")),
            (Some(0o755), Some(0o644), Some(0o755))
        );
    }

    #[test]
    fn pack_is_deterministic_and_round_trips() {
        let s = Scratch::new();
        let src = s.0.join("project");
        fs::create_dir_all(src.join("src/deep")).expect("tree");
        fs::create_dir_all(src.join("empty")).expect("empty dir");
        fs::write(src.join("README.md"), b"# hi\n").expect("file");
        fs::write(src.join("src/main.rs"), vec![7u8; 1500]).expect("file");
        fs::write(src.join("src/deep/z.bin"), b"").expect("empty file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(src.join("run.sh"), b"#!/bin/sh\n").expect("script");
            fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o700)).expect("chmod");
            std::os::unix::fs::symlink("/etc/passwd", src.join("passwd")).expect("symlink");
        }
        let mut first = Vec::new();
        let packed = pack_dir(&src, &mut first).expect("pack");
        let mut second = Vec::new();
        pack_dir(&src, &mut second).expect("pack again");
        assert_eq!(first, second, "the same tree packs to the same bytes");
        assert_eq!(packed.dirs, 4);
        #[cfg(unix)]
        assert_eq!(packed.skipped, ["project/passwd: symlink skipped"]);

        s.extract(&first, &Limits::default()).expect("extract");
        let out = s.out().join("project");
        assert_eq!(fs::read(out.join("README.md")).ok().as_deref(), Some(&b"# hi\n"[..]));
        assert_eq!(fs::read(out.join("src/main.rs")).ok(), Some(vec![7u8; 1500]));
        assert_eq!(fs::read(out.join("src/deep/z.bin")).ok(), Some(Vec::new()));
        assert!(out.join("empty").is_dir());
        assert!(
            fs::symlink_metadata(out.join("passwd")).is_err(),
            "symlinks never travel"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("run.sh"))
                .map(|m| m.permissions().mode() & 0o777)
                .ok();
            assert_eq!(mode, Some(0o755));
        }
        let mut long = Vec::new();
        fs::write(src.join("x".repeat(95)), b"").expect("long name");
        assert!(pack_dir(&src, &mut long).is_err_and(|e| e.contains("100")));
    }
}
