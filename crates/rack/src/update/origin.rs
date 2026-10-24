//! Where `rack update` gets releases: today the Racking service (`/dist`,
//! decision #21). Moving to another origin (e.g. release assets on a git host)
//! means rewriting this module behind the same two functions, `latest` and
//! `fetch`; the checks and the replacement in `update` stay as they are.
//!
//! What comes from the origin is untrusted until checked: the manifest is
//! validated here, and the caller verifies the bytes against it.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::version::Version;
use crate::client::{Client, RackError, Result};

/// Larger than any release build of `rack`; the service refuses bigger uploads too.
pub const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Which published builds count (decision #41): `Stable` only the releases
/// `X.Y.Z`, `Pre` the test builds `X.Y.Z-N` as well.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Pre,
}

/// One published binary, as the origin describes it.
pub struct Release {
    pub version: Version,
    /// The slot it was published in, which is also where it downloads from.
    pub channel: Channel,
    /// File name on the origin: `rack` or `rack.exe`.
    pub file: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

/// What the origin offers one target. `pre` is read only for `Channel::Pre`, so
/// the stable channel works whatever the test slot holds.
pub struct Available {
    pub stable: Option<Release>,
    pub pre: Option<Release>,
}

/// The manifest's top level is the stable slot (the shape clients 0.1.0 to 0.1.2
/// read); test builds sit in `prerelease`, a manifest of the same shape.
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    targets: HashMap<String, Entry>,
    #[serde(default)]
    prerelease: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Slot {
    #[serde(default)]
    targets: HashMap<String, Entry>,
}

#[derive(Deserialize)]
struct Entry {
    version: String,
    file: String,
    size: u64,
    sha256: String,
}

/// The releases published for `target` in the slots `channel` reads.
pub fn latest(client: &Client, target: &str, channel: Channel) -> Result<Available> {
    let manifest: serde_json::Value = client.get("/dist/manifest")?;
    parse_manifest(manifest, target, channel)
}

/// Whether the origin keeps test builds apart. A service from before the slots
/// would store a test build over the stable release.
pub fn has_test_slot(client: &Client) -> Result<bool> {
    let manifest: serde_json::Value = client.get("/dist/manifest")?;
    Ok(manifest.get("prerelease").is_some_and(serde_json::Value::is_object))
}

/// Downloads `release` for `target` into `dest`. The caller checks size and
/// SHA-256 against `release`; this only stops an answer past its declared size.
pub fn fetch(client: &Client, target: &str, release: &Release, dest: &Path) -> Result<()> {
    let path = match release.channel {
        Channel::Stable => format!("/dist/{target}/{}", release.file),
        Channel::Pre => format!("/dist/pre/{target}/{}", release.file),
    };
    client.download_path(&path, dest, Some(release.size))?;
    Ok(())
}

fn bad(what: impl std::fmt::Display) -> RackError {
    RackError::Transport(format!("the update manifest is invalid: {what}"))
}

fn parse_manifest(manifest: serde_json::Value, target: &str, channel: Channel) -> Result<Available> {
    let manifest: Manifest = serde_json::from_value(manifest).map_err(bad)?;
    let stable = manifest
        .targets
        .get(target)
        .map(|e| release(e, Channel::Stable))
        .transpose()?;
    let pre = match (channel, manifest.prerelease) {
        (Channel::Pre, Some(slot)) => {
            let slot: Slot = serde_json::from_value(slot).map_err(|e| bad(format!("prerelease: {e}")))?;
            slot.targets.get(target).map(|e| release(e, Channel::Pre)).transpose()?
        }
        _ => None,
    };
    Ok(Available { stable, pre })
}

fn release(entry: &Entry, channel: Channel) -> Result<Release> {
    let version = entry.version.parse().map_err(bad)?;
    // `file` goes into the download URL, so only the two names the service uses are taken.
    if entry.file != "rack" && entry.file != "rack.exe" {
        return Err(bad(format!("file `{}` is neither rack nor rack.exe", entry.file)));
    }
    if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("sha256 is not 64 hex characters"));
    }
    if entry.size == 0 || entry.size > MAX_BYTES {
        return Err(bad(format!("size {} is outside 1 to {MAX_BYTES} bytes", entry.size)));
    }
    Ok(Release {
        version,
        channel,
        file: entry.file.clone(),
        size: entry.size,
        sha256: entry.sha256.to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LINUX: &str = "x86_64-unknown-linux-gnu";

    fn manifest(entry: serde_json::Value) -> serde_json::Value {
        json!({"version": "0.2.0", "targets": {LINUX: entry}})
    }

    fn entry(version: &str) -> serde_json::Value {
        json!({"version": version, "file": "rack", "size": 10, "sha256": "ab".repeat(32)})
    }

    #[test]
    fn reads_the_own_target() {
        let sha = "AB".repeat(32);
        let got = parse_manifest(
            manifest(json!({"version": "0.2.0", "file": "rack", "size": 10, "sha256": sha})),
            LINUX,
            Channel::Stable,
        )
        .expect("valid");
        let release = got.stable.expect("published");
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!((release.file.as_str(), release.size), ("rack", 10));
        assert_eq!(release.sha256, "ab".repeat(32));
        assert!(release.channel == Channel::Stable);

        let none = parse_manifest(json!({"version": null, "targets": {}}), LINUX, Channel::Pre).expect("valid");
        assert!(none.stable.is_none() && none.pre.is_none());
        let other =
            parse_manifest(manifest(entry("0.2.0")), "aarch64-unknown-linux-gnu", Channel::Stable).expect("valid");
        assert!(other.stable.is_none());
    }

    #[test]
    fn reads_the_test_slot_only_on_the_pre_channel() {
        let m = json!({
            "version": "0.1.3",
            "targets": {LINUX: entry("0.1.3")},
            "prerelease": {"version": "0.1.4-2", "targets": {LINUX: entry("0.1.4-2")}},
        });
        let stable = parse_manifest(m.clone(), LINUX, Channel::Stable).expect("valid");
        assert!(stable.pre.is_none());
        let both = parse_manifest(m, LINUX, Channel::Pre).expect("valid");
        let pre = both.pre.expect("test build");
        assert_eq!(pre.version.to_string(), "0.1.4-2");
        assert!(pre.channel == Channel::Pre);
        assert_eq!(both.stable.map(|r| r.version.to_string()).as_deref(), Some("0.1.3"));

        // A broken test slot never stops a stable update.
        let broken = json!({"targets": {LINUX: entry("0.1.3")}, "prerelease": {"targets": {LINUX: entry("latest")}}});
        assert!(parse_manifest(broken.clone(), LINUX, Channel::Stable).is_ok());
        assert!(parse_manifest(broken, LINUX, Channel::Pre).is_err());
    }

    #[test]
    fn refuses_bad_entries() {
        let sha = "a".repeat(64);
        for entry in [
            json!({"version": "latest", "file": "rack", "size": 10, "sha256": sha}),
            json!({"version": "0.2.0", "file": "../rack", "size": 10, "sha256": sha}),
            json!({"version": "0.2.0", "file": "rack", "size": 10, "sha256": "abc"}),
            json!({"version": "0.2.0", "file": "rack", "size": 0, "sha256": sha}),
            json!({"version": "0.2.0", "file": "rack", "size": MAX_BYTES + 1, "sha256": sha}),
            json!({"version": "0.2.0", "file": "rack", "sha256": sha}),
        ] {
            assert!(
                parse_manifest(manifest(entry.clone()), LINUX, Channel::Stable).is_err(),
                "{entry}"
            );
        }
    }
}
