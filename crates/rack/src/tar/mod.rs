//! A small ustar writer and a strict ustar reader, enough to send a directory
//! as one attachment and unpack one safely. Only the POSIX ustar format
//! ([POSIX pax: ustar Interchange Format](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/pax.html#tag_20_94_13_06)):
//! regular files and directories, names of at most 100 bytes in the `name`
//! field, sizes in octal (so under 8 GiB). Pax and GNU extensions, links and
//! special files are refused by the reader and never written by the writer.

mod read;
mod write;

pub use read::{Extracted, Limits, Overwrite, extract};
pub use write::{Packed, pack_dir};

const BLOCK: usize = 512;
/// The `name` field; the writer leaves `prefix` empty and errs on longer paths
/// rather than reach for an extension.
const NAME_LEN: usize = 100;
/// The largest size eleven octal digits hold: one byte under 8 GiB.
const MAX_SIZE: u64 = 0o777_7777_7777;

const MAGIC: &[u8; 6] = b"ustar\0";
const VERSION: &[u8; 2] = b"00";

const TYPE_FILE: u8 = b'0';
const TYPE_DIR: u8 = b'5';

/// Field offsets and lengths of the ustar header.
mod field {
    pub const NAME: (usize, usize) = (0, 100);
    pub const MODE: (usize, usize) = (100, 8);
    pub const UID: (usize, usize) = (108, 8);
    pub const GID: (usize, usize) = (116, 8);
    pub const SIZE: (usize, usize) = (124, 12);
    pub const MTIME: (usize, usize) = (136, 12);
    pub const CHKSUM: (usize, usize) = (148, 8);
    pub const TYPEFLAG: usize = 156;
    pub const MAGIC: (usize, usize) = (257, 6);
    pub const VERSION: (usize, usize) = (263, 2);
    pub const DEVMAJOR: (usize, usize) = (329, 8);
    pub const DEVMINOR: (usize, usize) = (337, 8);
    pub const PREFIX: (usize, usize) = (345, 155);
}

/// The header checksum: the sum of all bytes with the checksum field read as spaces.
fn checksum(header: &[u8; BLOCK]) -> u64 {
    let (at, len) = field::CHKSUM;
    header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (at..at + len).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(*b)
            }
        })
        .sum()
}

/// Padding after `size` bytes of data, up to the next block.
fn padding(size: u64) -> u64 {
    (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64
}

/// Why an entry path is refused, or `None` when it is a plain relative path:
/// `/`-separated, no empty, `.` or `..` part, no backslash, colon or control
/// character, and no part Windows would treat as a device or silently change.
fn bad_path(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("empty name");
    }
    if path.starts_with('/') {
        return Some("absolute path");
    }
    if path.contains('\\') {
        return Some("backslash in the name (a Windows path separator)");
    }
    if path.contains(':') {
        return Some("colon in the name (a Windows drive or stream)");
    }
    if path.chars().any(char::is_control) {
        return Some("control character in the name");
    }
    for part in path.split('/') {
        match part {
            "" => return Some("empty path component"),
            "." | ".." => return Some("`.` or `..` component"),
            _ if part.ends_with('.') || part.ends_with(' ') => {
                return Some("component ending in a dot or space (Windows drops it)");
            }
            _ if windows_device(part) => return Some("Windows device name (CON, NUL, COM1...)"),
            _ => {}
        }
    }
    None
}

fn windows_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or_default().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_relative_paths_only() {
        for good in ["a", "dir/a.txt", "x/y/z", "console", "COM10", "a..b"] {
            assert_eq!(bad_path(good), None, "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a\\b",
            "..\\..\\x",
            "C:/x",
            "a:b",
            "a\nb",
            "aux",
            "Nul.txt",
            "com1",
            "LPT9.log",
            "dir./x",
            "x ",
        ] {
            assert!(bad_path(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn padding_to_blocks() {
        assert_eq!((padding(0), padding(1), padding(512), padding(513)), (0, 511, 0, 511));
    }
}
