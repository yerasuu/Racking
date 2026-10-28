//! Anchored wildcard patterns for the policy: `*` (any run of characters), `?` (one character)
//! and literal text, nothing else. Written in std instead of pulling in a regex crate, and kept this
//! small so a pattern means the same to the owner who writes it as to the check that applies it.

/// Whether `pattern` matches the whole of `text`.
#[must_use]
pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // The last `*` seen and the text position it currently stops at: classic single-star backtracking,
    // which is complete because a later star can always absorb what an earlier one would.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whether `pattern` matches the whole `path`, component by component, so a wildcard never
/// crosses a `/`: `/var/log/*.log` reaches `/var/log/syslog.log` but not `/var/log/x/y.log`.
/// Both sides are normalised paths (forward slashes).
#[must_use]
pub fn matches_path(pattern: &str, path: &str) -> bool {
    let mut p = pattern.split('/');
    let mut t = path.split('/');
    loop {
        match (p.next(), t.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if matches(a, b) => {}
            _ => return false,
        }
    }
}

/// Whether the text contains a wildcard, so callers know to match instead of compare.
#[must_use]
pub fn is_pattern(text: &str) -> bool {
    text.contains(['*', '?'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_text_only() {
        assert!(matches("--ok", "--ok"));
        assert!(!matches("--ok", "--ok2"), "no prefix match");
        assert!(!matches("--ok", "x--ok"), "no suffix match");
        assert!(matches("--n=*", "--n=5"));
        assert!(matches("--n=*", "--n="));
        assert!(!matches("--n=*", "--m=5"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("*", ""));
        assert!(matches("*.log", "a.b.log"));
        assert!(matches("a*b*c", "aXbYbZc"));
        assert!(!matches("a*b*c", "aXbYbZ"));
    }

    #[test]
    fn metacharacters_are_literal() {
        assert!(matches("[a]", "[a]"));
        assert!(!matches("[ab]", "a"), "no character classes");
        assert!(!matches("^--ok$", "--ok"), "no regex anchors");
        assert!(matches("$HOME", "$HOME"));
    }

    #[test]
    fn path_wildcards_stay_in_one_component() {
        assert!(matches_path("/var/log/*.log", "/var/log/sys.log"));
        assert!(!matches_path("/var/log/*.log", "/var/log/x/y.log"));
        assert!(!matches_path("/var/log/*", "/var/log"));
        assert!(matches_path("/w/*/out", "/w/a/out"));
    }
}
