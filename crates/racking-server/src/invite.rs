//! The pieces of a pairing invitation (#46) that need no storage: the one-time
//! code, its salted hash, the name proposed for the new agent and the fingerprint
//! both humans compare before the link is confirmed.

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::accounts::os_random;
use crate::error::{ApiError, ApiResult};
use crate::types::AgentName;

/// Default and longest life of an unused invite, in seconds.
pub const DEFAULT_TTL_SECS: i64 = 600;
pub const MAX_TTL_SECS: i64 = 3600;
/// Failed codes after which every live invite is voided, and after which a peer
/// address is refused for `FAILURE_WINDOW_SECS`.
pub const MAX_FAILURES: u32 = 5;
pub const FAILURE_WINDOW_SECS: i64 = 900;
/// How long the inviter has, once the code was redeemed, to compare the
/// fingerprints and confirm; past it the pending agent is denied.
pub const CONFIRM_SECS: i64 = 600;

/// No `0`/`O`, `1`/`I`/`L`: a human reads the code aloud or copies it from another
/// screen. 31 symbols, so the 16 symbols of a code carry 16 * log2(31) = 79.3 bits.
pub const ALPHABET: &[u8; 31] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const GROUPS: usize = 4;
const GROUP_LEN: usize = 4;
const CODE_LEN: usize = GROUPS * GROUP_LEN;
pub const SALT_BYTES: usize = 16;

/// A pairing code, `XXXX-XXXX-XXXX-XXXX`. No `Debug`, and its `Display` is the
/// secret, so it reaches a log only if someone formats it on purpose.
pub struct InviteCode([u8; CODE_LEN]);

impl InviteCode {
    /// A fresh code from the OS random source.
    pub fn generate() -> ApiResult<Self> {
        let mut code = [0u8; CODE_LEN];
        let mut filled = 0;
        let mut pool = [0u8; 32];
        while filled < CODE_LEN {
            os_random(&mut pool).map_err(|e| ApiError::internal("read the OS random source", &e))?;
            // Rejection sampling: bytes from 248 up would make the first symbols likelier.
            for b in pool.iter().copied().filter(|b| usize::from(*b) < ALPHABET.len() * 8) {
                if filled == CODE_LEN {
                    break;
                }
                code[filled] = ALPHABET[usize::from(b) % ALPHABET.len()];
                filled += 1;
            }
        }
        Ok(Self(code))
    }

    /// SHA-256 of the salt followed by the canonical code: the only form stored.
    pub fn hash(&self, salt: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(self.0);
        h.finalize().into()
    }
}

impl FromStr for InviteCode {
    type Err = ();

    /// Accepts any case, with or without dashes and spaces, so a code typed by hand
    /// still matches; anything else is refused without saying why, like a wrong code.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut code = [0u8; CODE_LEN];
        let mut n = 0;
        for b in s.bytes().filter(|b| !matches!(b, b'-' | b' ')) {
            let b = b.to_ascii_uppercase();
            if n == CODE_LEN || !ALPHABET.contains(&b) {
                return Err(());
            }
            code[n] = b;
            n += 1;
        }
        if n == CODE_LEN { Ok(Self(code)) } else { Err(()) }
    }
}

impl fmt::Display for InviteCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, group) in self.0.chunks(GROUP_LEN).enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            // The alphabet is ASCII, so every group is valid UTF-8.
            f.write_str(std::str::from_utf8(group).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

/// Constant-time equality of two digests.
pub fn same_digest(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a presented code is the one hashed as `hash` with `salt`. Text that is
/// no code at all is hashed too, as a filler that matches nothing, so a malformed
/// code takes as long to refuse as a wrong one.
pub fn matches(presented: Option<&InviteCode>, salt: &[u8], hash: &[u8; 32]) -> bool {
    let filler = InviteCode([0; CODE_LEN]);
    let code = presented.unwrap_or(&filler);
    same_digest(&code.hash(salt), hash) && presented.is_some()
}

pub fn new_salt() -> ApiResult<[u8; SALT_BYTES]> {
    let mut salt = [0u8; SALT_BYTES];
    os_random(&mut salt).map_err(|e| ApiError::internal("read the OS random source", &e))?;
    Ok(salt)
}

/// `agent-` and 4 random symbols, for an invite that names no agent.
pub fn proposed_name() -> ApiResult<AgentName> {
    const SYMBOLS: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
    let mut bytes = [0u8; 4];
    os_random(&mut bytes).map_err(|e| ApiError::internal("read the OS random source", &e))?;
    let suffix: String = bytes
        .iter()
        .map(|b| char::from(SYMBOLS[usize::from(*b) % SYMBOLS.len()]))
        .collect();
    format!("agent-{suffix}")
        .parse()
        .map_err(|e: String| ApiError::internal("propose an agent name", &e))
}

/// Four words both screens show: from the invite id, the invited agent's name and
/// the first bytes of its token's SHA-256. A code redeemed by someone else gives
/// another name or token, so the words differ from those on the joiner's screen.
pub fn fingerprint(invite: i64, name: &AgentName, token_digest: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"racking-fingerprint-v1\0");
    h.update(invite.to_be_bytes());
    h.update(name.as_str());
    h.update([0]);
    h.update(&token_digest[..8]);
    let d = h.finalize();
    d[..4]
        .iter()
        .map(|b| WORDS[usize::from(*b)])
        .collect::<Vec<_>>()
        .join(" ")
}

/// 256 short, distinct English words: one byte each, so 4 words carry 32 bits.
pub const WORDS: [&str; 256] = [
    "able", "acid", "aged", "also", "apex", "arch", "area", "army", "atom", "aunt", "auto", "away", "axis", "baby",
    "back", "bake", "ball", "band", "bank", "barn", "base", "bath", "beam", "bean", "bear", "beef", "bell", "belt",
    "bend", "best", "bike", "bird", "blue", "boat", "body", "bold", "bolt", "bone", "book", "boot", "born", "boss",
    "both", "bowl", "bulb", "bush", "busy", "cake", "calm", "camp", "card", "care", "cart", "case", "cash", "cave",
    "cell", "chef", "chin", "city", "clay", "clip", "club", "coal", "coat", "code", "coin", "cold", "cook", "cool",
    "copy", "cord", "corn", "cost", "crew", "crop", "cube", "cup", "curl", "dark", "data", "dawn", "deal", "deck",
    "deep", "deer", "desk", "dial", "diet", "dish", "dock", "doll", "door", "dove", "draw", "drum", "duck", "dust",
    "duty", "earn", "east", "easy", "echo", "edge", "epic", "even", "exit", "face", "fact", "fair", "farm", "fast",
    "fern", "film", "fire", "firm", "fish", "flag", "flat", "flow", "foam", "fog", "folk", "food", "foot", "fork",
    "form", "fort", "free", "frog", "fuel", "gain", "game", "gate", "gear", "gift", "glow", "glue", "goal", "gold",
    "golf", "good", "gown", "grid", "grow", "gulf", "hair", "half", "hall", "hand", "harp", "hawk", "heat", "herb",
    "hero", "high", "hill", "hint", "hive", "hold", "home", "hook", "hope", "horn", "host", "hour", "huge", "hunt",
    "idea", "inch", "iron", "item", "jade", "jazz", "jeep", "joke", "jump", "just", "keen", "kept", "kick", "kind",
    "king", "kite", "knee", "knot", "lake", "lamb", "lamp", "land", "lane", "last", "lawn", "leaf", "lean", "left",
    "lens", "life", "lift", "lime", "line", "link", "lion", "list", "loaf", "lock", "loft", "logo", "long", "loop",
    "lord", "love", "luck", "lung", "made", "mail", "main", "make", "malt", "mane", "maple", "mark", "mask", "mast",
    "meal", "meat", "melt", "menu", "mild", "milk", "mill", "mind", "mint", "mist", "moat", "mode", "mole", "moon",
    "moss", "moth", "much", "mule", "nail", "name", "navy", "neat", "neck", "nest", "news", "next", "nice", "node",
    "noon", "norm", "nose", "note",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> AgentName {
        n.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    #[test]
    fn codes_use_the_unambiguous_alphabet_in_four_groups() {
        for _ in 0..50 {
            let code = InviteCode::generate().expect("code").to_string();
            assert_eq!(code.len(), 19, "{code}");
            let groups: Vec<&str> = code.split('-').collect();
            assert_eq!(groups.len(), 4);
            assert!(groups.iter().all(|g| g.len() == 4));
            assert!(
                code.bytes().filter(|b| *b != b'-').all(|b| ALPHABET.contains(&b)),
                "{code}"
            );
            for bad in *b"0O1IL" {
                assert!(!code.as_bytes().contains(&bad), "{code}");
            }
        }
        assert!(f64::from(16u32) * f64::from(31u32).log2() > 79.0);
        let a = InviteCode::generate().expect("code").to_string();
        let b = InviteCode::generate().expect("code").to_string();
        assert_ne!(a, b);
    }

    #[test]
    fn typed_codes_are_normalised_and_bad_ones_refused() {
        let code = InviteCode::generate().expect("code");
        let text = code.to_string();
        let typed = text.to_ascii_lowercase().replace('-', " ");
        let parsed: InviteCode = typed.parse().expect("lowercase with spaces");
        assert_eq!(parsed.to_string(), text);
        for bad in [
            "",
            "ABCD-EFGH",
            "ABCD-EFGH-JKMN-PQRS-T",
            "0BCD-EFGH-JKMN-PQRS",
            "ABCD_EFGH_JKMN_PQRS",
        ] {
            assert!(bad.parse::<InviteCode>().is_err(), "{bad}");
        }
    }

    #[test]
    fn hashes_depend_on_the_salt_and_the_code() {
        let code: InviteCode = "2345-6789-ABCD-EFGH".parse().expect("code");
        let other: InviteCode = "2345-6789-ABCD-EFGJ".parse().expect("code");
        let (s1, s2) = ([1u8; SALT_BYTES], [2u8; SALT_BYTES]);
        assert_eq!(code.hash(&s1), code.hash(&s1));
        assert!(same_digest(&code.hash(&s1), &code.hash(&s1)));
        assert!(!same_digest(&code.hash(&s1), &code.hash(&s2)));
        assert!(!same_digest(&code.hash(&s1), &other.hash(&s1)));
        assert_ne!(new_salt().expect("salt"), new_salt().expect("salt"));
        assert!(matches(Some(&code), &s1, &code.hash(&s1)));
        assert!(!matches(Some(&other), &s1, &code.hash(&s1)));
        let filler = InviteCode([0; CODE_LEN]).hash(&s1);
        assert!(
            !matches(None, &s1, &filler),
            "no text ever matches, even the filler's own hash"
        );
    }

    #[test]
    fn fingerprints_are_stable_and_follow_every_input() {
        let digest = [7u8; 32];
        let base = fingerprint(3, &name("mac"), &digest);
        assert_eq!(base, fingerprint(3, &name("mac"), &digest), "stable");
        assert_eq!(base.split(' ').count(), 4);
        assert!(base.split(' ').all(|w| WORDS.contains(&w)));
        assert_ne!(base, fingerprint(4, &name("mac"), &digest), "invite id");
        assert_ne!(base, fingerprint(3, &name("mad"), &digest), "name");
        let mut other = digest;
        other[0] = 8;
        assert_ne!(base, fingerprint(3, &name("mac"), &other), "token hash");
        let mut tail = digest;
        tail[31] = 9;
        assert_eq!(base, fingerprint(3, &name("mac"), &tail), "only the prefix counts");
    }

    #[test]
    fn the_word_list_is_256_distinct_short_words() {
        let mut sorted = WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 256);
        assert!(
            WORDS
                .iter()
                .all(|w| (3..=5).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()))
        );
    }

    #[test]
    fn proposed_names_are_agent_names() {
        let n = proposed_name().expect("name");
        assert!(n.as_str().starts_with("agent-") && n.as_str().len() == 10, "{n}");
    }
}
