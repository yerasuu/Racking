//! Packs a directory into a deterministic ustar archive: entries sorted by
//! name, owner 0, time 0, modes 0o755 or 0o644, so packing the same tree
//! twice gives the same bytes (and the same SHA-256, which a resumed upload
//! relies on).

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use super::{BLOCK, MAGIC, MAX_SIZE, NAME_LEN, TYPE_DIR, TYPE_FILE, VERSION, bad_path, checksum, field, padding};

/// What went into an archive, and what was left out with a warning.
#[derive(Debug, Default)]
pub struct Packed {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Symlinks and special files, which the archive never carries.
    pub skipped: Vec<String>,
}

/// Writes `dir` and everything under it to `out`, each path starting with the
/// directory's own name. Errors on a path the reader would refuse or ustar
/// cannot hold (over 100 bytes), on a file of 8 GiB or more, and on a file
/// that changes size while it is read.
pub fn pack_dir(dir: &Path, out: &mut dyn Write) -> Result<Packed, String> {
    let root = dir
        .canonicalize()
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    let top = root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no usable name for the archive", dir.display()))?
        .to_owned();
    let mut packed = Packed::default();
    add_dir(&root, &top, out, &mut packed)?;
    out.write_all(&[0u8; 2 * BLOCK])
        .map_err(|e| format!("write the archive: {e}"))?;
    Ok(packed)
}

fn add_dir(path: &Path, name: &str, out: &mut dyn Write, packed: &mut Packed) -> Result<(), String> {
    write_header(out, &format!("{name}/"), TYPE_DIR, 0o755, 0)?;
    packed.dirs += 1;
    let mut children = fs::read_dir(path)
        .and_then(|entries| {
            entries
                .map(|e| e.map(|e| e.file_name()))
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|e| format!("cannot list {}: {e}", path.display()))?;
    children.sort();
    for child in children {
        let child_path = path.join(&child);
        let child_name = child
            .to_str()
            .map(|c| format!("{name}/{c}"))
            .ok_or_else(|| format!("{} is not a UTF-8 name; rename it first", child_path.display()))?;
        let meta =
            fs::symlink_metadata(&child_path).map_err(|e| format!("cannot read {}: {e}", child_path.display()))?;
        let kind = meta.file_type();
        if kind.is_dir() {
            add_dir(&child_path, &child_name, out, packed)?;
        } else if kind.is_file() {
            add_file(&child_path, &child_name, meta.len(), executable(&meta), out)?;
            packed.files += 1;
            packed.bytes += meta.len();
        } else {
            let what = if kind.is_symlink() { "symlink" } else { "special file" };
            packed.skipped.push(format!("{child_name}: {what} skipped"));
        }
    }
    Ok(())
}

fn add_file(path: &Path, name: &str, size: u64, exec: bool, out: &mut dyn Write) -> Result<(), String> {
    if size > MAX_SIZE {
        return Err(format!(
            "{} is {size} bytes; a ustar archive holds files under 8 GiB, attach it on its own",
            path.display()
        ));
    }
    write_header(out, name, TYPE_FILE, if exec { 0o755 } else { 0o644 }, size)?;
    let file = fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let copied = io::copy(&mut file.take(size), out).map_err(|e| format!("cannot pack {}: {e}", path.display()))?;
    if copied != size {
        return Err(format!("{} changed while it was packed; try again", path.display()));
    }
    let pad = usize::try_from(padding(size)).unwrap_or(0);
    out.write_all(&[0u8; BLOCK][..pad])
        .map_err(|e| format!("write the archive: {e}"))
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    false
}

fn write_header(out: &mut dyn Write, name: &str, kind: u8, mode: u32, size: u64) -> Result<(), String> {
    let path = name.strip_suffix('/').unwrap_or(name);
    if let Some(why) = bad_path(path) {
        return Err(format!("`{name}` cannot go in the archive: {why}"));
    }
    if name.len() > NAME_LEN {
        return Err(format!(
            "`{name}` is {} bytes; ustar names hold {NAME_LEN}: shorten the path, or pack it with another tool",
            name.len()
        ));
    }
    out.write_all(&header(name, kind, mode, size))
        .map_err(|e| format!("write the archive: {e}"))
}

/// One ustar header; `name` must fit its field. Also what the reader's tests build their fixtures from.
pub(super) fn header(name: &str, kind: u8, mode: u32, size: u64) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    put(&mut h, field::NAME, name.as_bytes());
    octal(&mut h, field::MODE, u64::from(mode));
    octal(&mut h, field::UID, 0);
    octal(&mut h, field::GID, 0);
    octal(&mut h, field::SIZE, size);
    octal(&mut h, field::MTIME, 0);
    h[field::TYPEFLAG] = kind;
    put(&mut h, field::MAGIC, MAGIC);
    put(&mut h, field::VERSION, VERSION);
    octal(&mut h, field::DEVMAJOR, 0);
    octal(&mut h, field::DEVMINOR, 0);
    seal(&mut h);
    h
}

/// Writes the checksum: six octal digits, a NUL and a space, as most tars do.
pub(super) fn seal(h: &mut [u8; BLOCK]) {
    let sum = checksum(h);
    let (at, _) = field::CHKSUM;
    h[at..at + 8].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
}

fn put(h: &mut [u8; BLOCK], (at, len): (usize, usize), bytes: &[u8]) {
    let n = bytes.len().min(len);
    h[at..at + n].copy_from_slice(&bytes[..n]);
}

/// Zero-filled octal over all but the last byte of the field, which stays NUL.
fn octal(h: &mut [u8; BLOCK], (at, len): (usize, usize), value: u64) {
    let text = format!("{value:0width$o}", width = len - 1);
    put(h, (at, len - 1), text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_carry_a_valid_checksum_and_fixed_fields() {
        let h = header("dir/a.txt", TYPE_FILE, 0o644, 1234);
        assert_eq!(&h[124..136], b"00000002322\0");
        assert_eq!(&h[100..108], b"0000644\0");
        assert_eq!(&h[136..148], b"00000000000\0", "time 0 keeps archives deterministic");
        let stored = std::str::from_utf8(&h[148..154]).expect("ascii");
        assert_eq!(u64::from_str_radix(stored, 8).ok(), Some(checksum(&h)));
        assert_eq!(&h[257..265], b"ustar\x0000");
    }

    #[test]
    fn long_and_bad_names_are_errors() {
        let mut out = Vec::new();
        let long = format!("{}/x", "d".repeat(99));
        assert!(write_header(&mut out, &long, TYPE_FILE, 0o644, 0).is_err_and(|e| e.contains("100")));
        assert!(write_header(&mut out, "a\\b", TYPE_FILE, 0o644, 0).is_err());
        assert!(out.is_empty());
    }
}
