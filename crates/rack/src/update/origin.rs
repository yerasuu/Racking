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

/// One published binary, as the origin describes it.
pub struct Release {
    pub version: Version,
    /// File name on the origin: `rack` or `rack.exe`.
    pub file: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
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

/// The release published for `target`, or `None` when the origin has none.
pub fn latest(client: &Client, target: &str) -> Result<Option<Release>> {
    let manifest: serde_json::Value = client.get("/dist/manifest")?;
    parse_manifest(manifest, target)
}

/// Downloads `release` for `target` into `dest`. The caller checks size and
/// SHA-256 against `release`; this only stops an answer past its declared size.
pub fn fetch(client: &Client, target: &str, release: &Release, dest: &Path) -> Result<()> {
    client.download_path(&format!("/dist/{target}/{}", release.file), dest, Some(release.size))?;
    Ok(())
}

fn parse_manifest(manifest: serde_json::Value, target: &str) -> Result<Option<Release>> {
    let bad = |what: String| RackError::Transport(format!("the update manifest is invalid: {what}"));
    let manifest: Manifest = serde_json::from_value(manifest).map_err(|e| bad(e.to_string()))?;
    let Some(entry) = manifest.targets.get(target) else {
        return Ok(None);
    };
    let version = entry.version.parse().map_err(bad)?;
    // `file` goes into the download URL, so only the two names the service uses are taken.
    if entry.file != "rack" && entry.file != "rack.exe" {
        return Err(bad(format!("file `{}` is neither rack nor rack.exe", entry.file)));
    }
    if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("sha256 is not 64 hex characters".to_owned()));
    }
    if entry.size == 0 || entry.size > MAX_BYTES {
        return Err(bad(format!("size {} is outside 1 to {MAX_BYTES} bytes", entry.size)));
    }
    Ok(Some(Release {
        version,
        file: entry.file.clone(),
        size: entry.size,
        sha256: entry.sha256.to_ascii_lowercase(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LINUX: &str = "x86_64-unknown-linux-gnu";

    fn manifest(entry: serde_json::Value) -> serde_json::Value {
        json!({"version": "0.2.0", "targets": {LINUX: entry}})
    }

    #[test]
    fn reads_the_own_target() {
        let sha = "AB".repeat(32);
        let got = parse_manifest(
            manifest(json!({"version": "0.2.0", "file": "rack", "size": 10, "sha256": sha})),
            LINUX,
        )
        .expect("valid");
        let release = got.expect("published");
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!((release.file.as_str(), release.size), ("rack", 10));
        assert_eq!(release.sha256, "ab".repeat(32));

        let none = parse_manifest(json!({"version": null, "targets": {}}), LINUX).expect("valid");
        assert!(none.is_none());
        let other = parse_manifest(
            manifest(json!({"version": "0.2.0", "file": "rack", "size": 10, "sha256": sha})),
            "aarch64-unknown-linux-gnu",
        )
        .expect("valid");
        assert!(other.is_none());
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
            assert!(parse_manifest(manifest(entry.clone()), LINUX).is_err(), "{entry}");
        }
    }
}
