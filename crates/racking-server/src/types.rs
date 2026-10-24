//! Validated values that cross the HTTP edge. Each newtype is built once from
//! untrusted text, so code past the edge can rely on its shape.

use std::fmt;
use std::str::FromStr;

/// Agent name, `[a-z0-9][a-z0-9-]{0,31}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentName(String);

impl AgentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AgentName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = s.bytes();
        let first_ok = bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let rest_ok = bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if first_ok && rest_ok && s.len() <= 32 {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("agent name `{s}` must match [a-z0-9][a-z0-9-]{{0,31}}"))
        }
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Job id: `j` followed by a positive integer. Stored as the integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(i64);

impl JobId {
    pub fn new(n: i64) -> Self {
        Self(n)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl FromStr for JobId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.strip_prefix('j')
            .filter(|digits| !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .map(Self)
            .ok_or_else(|| format!("job id `{}` must look like j42", truncate(s, 40)))
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "j{}", self.0)
    }
}

/// Attachment file name, `[A-Za-z0-9._-]{1,100}`, never `.` or `..`, so it is
/// safe to join to a directory path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentName(String);

impl AttachmentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AttachmentName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let charset_ok = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if charset_ok && (1..=100).contains(&s.len()) && s != "." && s != ".." {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "attachment name `{}` must match [A-Za-z0-9._-]{{1,100}} and not be . or ..",
                truncate(s, 40)
            ))
        }
    }
}

/// Inbox position or update counter. Cursors are never negative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(i64);

impl Seq {
    #[cfg(test)]
    pub fn new(n: i64) -> Self {
        Self(n)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl FromStr for Seq {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(Self)
            .ok_or_else(|| format!("cursor `{}` must be a non-negative integer", truncate(s, 40)))
    }
}

/// Lowercase hex SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn of_digest(digest: &[u8]) -> Self {
        Self(hex(digest))
    }
}

impl FromStr for Sha256Hex {
    type Err = String;

    /// Accepts either case so clients using uppercase tools are not rejected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(s.to_ascii_lowercase()))
        } else {
            Err("X-Sha256 must be 64 hex characters".to_owned())
        }
    }
}

/// A build target `rack` is published for. The list is fixed, so a target is
/// safe to use as a directory name and nothing else reaches the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LinuxX86_64,
    WindowsX86_64,
    LinuxAarch64,
}

impl Target {
    pub const ALL: [Target; 3] = [Target::LinuxX86_64, Target::WindowsX86_64, Target::LinuxAarch64];

    /// The Rust target triple, as `rack` embeds it at build time.
    pub fn as_str(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Target::WindowsX86_64 => "x86_64-pc-windows-gnu",
            Target::LinuxAarch64 => "aarch64-unknown-linux-gnu",
        }
    }

    pub fn exe_name(self) -> &'static str {
        match self {
            Target::WindowsX86_64 => "rack.exe",
            Target::LinuxX86_64 | Target::LinuxAarch64 => "rack",
        }
    }
}

impl FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Target::ALL.into_iter().find(|t| t.as_str() == s).ok_or_else(|| {
            let known = Target::ALL.map(Target::as_str).join(", ");
            format!("target `{}` is not one of {known}", truncate(s, 60))
        })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A version that follows the grammar of [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html):
/// `MAJOR.MINOR.PATCH`, an optional `-pre.release` and `+build` metadata. The
/// service only stores and shows it; clients do the ordering.
pub struct Version(String);

impl Version {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ident_ok = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let number_ok =
            |id: &str| id == "0" || (!id.is_empty() && !id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()));
        let (rest, build) = s.split_once('+').map_or((s, None), |(r, b)| (r, Some(b)));
        let (core, pre) = rest.split_once('-').map_or((rest, None), |(c, p)| (c, Some(p)));
        let mut parts = core.split('.');
        let core_ok = (0..3).all(|_| parts.next().is_some_and(|p| number_ok(p) && p.parse::<u64>().is_ok()))
            && parts.next().is_none();
        // Numeric pre-release identifiers must not have leading zeros; alphanumeric ones may.
        let pre_ok = pre.is_none_or(|p| {
            p.split('.')
                .all(|id| ident_ok(id) && (number_ok(id) || !id.bytes().all(|b| b.is_ascii_digit())))
        });
        let build_ok = build.is_none_or(|b| b.split('.').all(ident_ok));
        if s.len() <= 64 && core_ok && pre_ok && build_ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "version `{}` must be semver, e.g. 1.2.3 or 1.2.3-rc.1, at most 64 chars",
                truncate(s, 70)
            ))
        }
    }
}

/// What an agent says about itself in `GET /agents`: one line of at most
/// `MAX_CHARS` chars. Other agents read it, so it is kept short and single-line.
pub struct About(String);

impl About {
    pub const MAX_CHARS: usize = 200;

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl FromStr for About {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.chars().count() <= Self::MAX_CHARS && !s.chars().any(char::is_control) {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("at most {} chars on one line", Self::MAX_CHARS))
        }
    }
}

/// A short label an agent reports about its own software (`rack_version`, `os`):
/// 1 to `MAX_CHARS` of `A-Za-z0-9._+-` and spaces.
pub struct Label(String);

impl Label {
    pub const MAX_CHARS: usize = 40;

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl FromStr for Label {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let ok = (1..=Self::MAX_CHARS).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b' '));
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("1 to {} of A-Za-z0-9._+- and spaces", Self::MAX_CHARS))
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Cuts `s` to at most `max` chars, on a char boundary, for echoing input in errors.
pub fn truncate(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(i, _)| &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names() {
        assert!("linux".parse::<AgentName>().is_ok());
        assert!("w-1".parse::<AgentName>().is_ok());
        assert!("-x".parse::<AgentName>().is_err());
        assert!("Linux".parse::<AgentName>().is_err());
        assert!("".parse::<AgentName>().is_err());
        assert!("a".repeat(33).parse::<AgentName>().is_err());
    }

    #[test]
    fn job_ids() {
        assert_eq!("j42".parse::<JobId>().map(JobId::get), Ok(42));
        assert!("j0".parse::<JobId>().is_err());
        assert!("j".parse::<JobId>().is_err());
        assert!("42".parse::<JobId>().is_err());
        assert!("j-1".parse::<JobId>().is_err());
        assert!("j99999999999999999999".parse::<JobId>().is_err());
        assert_eq!(JobId::new(7).to_string(), "j7");
    }

    #[test]
    fn attachment_names() {
        assert!("probe.log".parse::<AttachmentName>().is_ok());
        assert!("..".parse::<AttachmentName>().is_err());
        assert!("a/b".parse::<AttachmentName>().is_err());
        assert!("a\\b".parse::<AttachmentName>().is_err());
        assert!("".parse::<AttachmentName>().is_err());
        assert!("x".repeat(101).parse::<AttachmentName>().is_err());
    }

    #[test]
    fn targets() {
        for t in Target::ALL {
            assert_eq!(t.as_str().parse::<Target>(), Ok(t));
        }
        assert_eq!(
            "x86_64-pc-windows-gnu".parse::<Target>().map(Target::exe_name),
            Ok("rack.exe")
        );
        assert!("x86_64-apple-darwin".parse::<Target>().is_err());
        assert!("../etc".parse::<Target>().is_err());
    }

    #[test]
    fn versions() {
        for ok in [
            "0.1.0",
            "1.2.3",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0-0.3.7",
            "1.0.0-x-y.z",
            "1.0.0+build.5",
        ] {
            assert!(ok.parse::<Version>().is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-",
            "1.2.3-01",
            "1.2.3+",
            "1.2.3-a..b",
            "v1.2.3",
            "1.2.x",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }

    #[test]
    fn sha_and_truncate() {
        let upper = "A".repeat(64);
        assert_eq!(
            upper.parse::<Sha256Hex>().map(|h| h.as_str().to_owned()),
            Ok("a".repeat(64))
        );
        assert!("zz".parse::<Sha256Hex>().is_err());
        assert_eq!(truncate("añb", 2), "añ");
        assert_eq!(hex(&[0, 255]), "00ff");
    }

    #[test]
    fn about_and_labels() {
        assert_eq!(
            "  on the Windows PC  ".parse::<About>().map(About::into_inner),
            Ok("on the Windows PC".to_owned())
        );
        assert!(
            "ñ".repeat(About::MAX_CHARS).parse::<About>().is_ok(),
            "counted in chars, not bytes"
        );
        assert!("x".repeat(About::MAX_CHARS + 1).parse::<About>().is_err());
        assert!("two\nlines".parse::<About>().is_err());
        assert!("".parse::<About>().is_ok());
        assert!("0.1.0".parse::<Label>().is_ok());
        assert!("windows x86_64".parse::<Label>().is_ok());
        assert!("".parse::<Label>().is_err());
        assert!("a;b".parse::<Label>().is_err());
        assert!("x".repeat(Label::MAX_CHARS + 1).parse::<Label>().is_err());
    }
}
