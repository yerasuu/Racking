//! Versions as [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)
//! defines them, with its precedence rules, so `rack update` never installs an
//! older or equal release.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A checked semver version. Equality and order follow semver precedence,
/// which ignores build metadata (`1.0.0+a` equals `1.0.0+b`).
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
    text: String,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    // Declared first so numeric identifiers sort below alphanumeric ones, as semver item 11.4.3 asks.
    Num(u64),
    Alpha(String),
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("`{s}` is not a semver version such as 1.2.3 or 1.2.3-rc.1");
        let ident_ok = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let number = |id: &str| {
            let digits = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
            (digits && (id == "0" || !id.starts_with('0')))
                .then(|| id.parse::<u64>().ok())
                .flatten()
        };
        if s.len() > 64 {
            return Err(bad());
        }
        let (rest, build) = s.split_once('+').map_or((s, None), |(r, b)| (r, Some(b)));
        if build.is_some_and(|b| !b.split('.').all(ident_ok)) {
            return Err(bad());
        }
        let (core_text, pre_text) = rest.split_once('-').map_or((rest, None), |(c, p)| (c, Some(p)));
        let mut parts = core_text.split('.').map(number);
        let mut core = [0u64; 3];
        for slot in &mut core {
            *slot = parts.next().flatten().ok_or_else(bad)?;
        }
        if parts.next().is_some() {
            return Err(bad());
        }
        let pre = match pre_text {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| match number(id) {
                    Some(n) => Ok(Ident::Num(n)),
                    // An all-digit identifier that is not a valid number has a leading zero or overflows.
                    None if ident_ok(id) && !id.bytes().all(|b| b.is_ascii_digit()) => Ok(Ident::Alpha(id.to_owned())),
                    None => Err(bad()),
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Self {
            core,
            pre,
            text: s.to_owned(),
        })
    }
}

impl Version {
    /// `X.Y.Z-N`: a test build, ordered below the stable `X.Y.Z`.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // A pre-release sorts below its release (1.0.0-rc.1 < 1.0.0).
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    #[test]
    fn precedence() {
        // The chain from semver item 11.4, plus core and build cases.
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "1.10.0",
            "2.0.0",
        ];
        for pair in chain.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.0.0+linux"), v("1.0.0+other"));
        assert!(v("0.10.0") > v("0.9.9"));

        // The numbering of decision #41: test builds X.Y.Z-N before the stable X.Y.Z.
        let tests = ["0.1.3", "0.1.4-1", "0.1.4-2", "0.1.4-10", "0.1.4", "0.1.5-1"];
        for pair in tests.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert!(v("0.1.4-1").is_prerelease() && !v("0.1.4").is_prerelease() && !v("0.1.4+a-b").is_prerelease());
    }

    #[test]
    fn grammar() {
        // The same table as racking-server's types::Version tests, so both parsers accept the same grammar.
        let mut cases = 0;
        for line in include_str!("../../../../testdata/versions.txt").lines() {
            let Some((expect, s)) = line.split_once(' ').filter(|_| !line.starts_with('#')) else {
                continue;
            };
            let s = if s == "<empty>" { "" } else { s };
            match expect {
                "ok" => assert_eq!(v(s).to_string(), s),
                "bad" => assert!(s.parse::<Version>().is_err(), "{s}"),
                other => panic!("unknown case `{other}`"),
            }
            cases += 1;
        }
        assert!(cases > 20, "the table was read");
    }
}
