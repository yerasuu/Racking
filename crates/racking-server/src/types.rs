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
}
