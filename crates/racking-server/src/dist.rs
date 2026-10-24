//! `rack` binaries served for `rack update` (decision #21): the service is the
//! update origin on the LAN. Each target has two slots (decision #41): the
//! stable release under `<data_dir>/dist/<target>/` and the latest test build
//! (a SemVer pre-release, `X.Y.Z-N`) under `<data_dir>/dist/pre/<target>/`,
//! each one file next to a `meta.json` with its version, size and SHA-256. The
//! version of an upload picks its slot, so a test build never replaces the
//! stable release nor the other way round. A stable publish leaves the pre slot
//! alone: clients ignore it once it is older than the stable one, and the
//! service does not order versions. The SHA-256 is the only integrity check:
//! nothing is signed.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::store::{Download, limits, receive_file};
use crate::types::{Sha256Hex, Target, Version};

const META: &str = "meta.json";

#[derive(Clone, Copy)]
pub enum Slot {
    Stable,
    Pre,
}

impl Slot {
    pub fn of(version: &Version) -> Self {
        if version.is_prerelease() {
            Slot::Pre
        } else {
            Slot::Stable
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Stable => "stable",
            Slot::Pre => "pre",
        }
    }

    /// Download path of `target` in this slot, as clients call it.
    pub fn url_path(self, target: Target) -> String {
        match self {
            Slot::Stable => format!("/dist/{target}/{}", target.exe_name()),
            Slot::Pre => format!("/dist/pre/{target}/{}", target.exe_name()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Meta {
    version: String,
    size: u64,
    sha256: String,
    published_at: i64,
}

pub struct Dist {
    dir: PathBuf,
    /// Readers take the meta and open the file under the read lock, and a
    /// publish swaps both under the write lock, so nobody sees one without the other.
    lock: RwLock<()>,
    tmp_counter: AtomicU64,
}

impl Dist {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let dir = data_dir.join("dist");
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(Self {
            dir,
            lock: RwLock::new(()),
            tmp_counter: AtomicU64::new(0),
        })
    }

    /// `{"version", "targets": {<triple>: {"version", "file", "size", "sha256"}}, "prerelease": {same}}`.
    /// The top level is the stable slot, in the shape clients 0.1.0 to 0.1.2 read;
    /// `prerelease` is added beside it so those clients, which ignore unknown keys,
    /// never see a test build. Each `version` is the one published last in its slot,
    /// null while nothing is.
    pub fn manifest(&self) -> ApiResult<Value> {
        let _read = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        let mut manifest = self.slot_manifest(Slot::Stable)?;
        manifest["prerelease"] = self.slot_manifest(Slot::Pre)?;
        Ok(manifest)
    }

    fn slot_manifest(&self, slot: Slot) -> ApiResult<Value> {
        let mut targets = Map::new();
        let mut newest: Option<(i64, String)> = None;
        for target in Target::ALL {
            let Some(meta) = self.meta(slot, target)? else {
                continue;
            };
            if newest.as_ref().is_none_or(|(at, _)| meta.published_at >= *at) {
                newest = Some((meta.published_at, meta.version.clone()));
            }
            targets.insert(
                target.as_str().to_owned(),
                json!({"version": meta.version, "file": target.exe_name(), "size": meta.size, "sha256": meta.sha256}),
            );
        }
        Ok(json!({"version": newest.map(|(_, v)| v), "targets": targets}))
    }

    pub fn download(&self, slot: Slot, target: Target) -> ApiResult<Download> {
        let _read = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        let meta = self.meta(slot, target)?.ok_or_else(|| {
            let what = match slot {
                Slot::Stable => "rack",
                Slot::Pre => "rack test build",
            };
            ApiError::new(
                Code::NotFound,
                format!("no {what} published for {target}"),
                "GET /dist/manifest lists the published targets; an admin agent publishes with `rack publish`.",
            )
        })?;
        let file = File::open(self.slot_dir(slot, target).join(target.exe_name()))
            .map_err(|e| ApiError::internal("open the rack binary", &e))?;
        Ok(Download {
            file,
            sha256: meta.sha256,
        })
    }

    /// Streams the upload next to its final place, checks size and hash, then
    /// replaces the binary and meta of the slot the version picks. The caller has
    /// checked the agent may publish and that the version has no build metadata.
    pub fn publish(
        &self,
        target: Target,
        version: &Version,
        body: &mut dyn Read,
        claimed: &Sha256Hex,
    ) -> ApiResult<Value> {
        let slot = Slot::of(version);
        let dir = self.slot_dir(slot, target);
        fs::create_dir_all(&dir).map_err(|e| ApiError::internal("create the dist directory", &e))?;
        // Same directory as the final file, so the rename stays on one file system and is atomic.
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".upload-{}-{n}", std::process::id()));
        let tmp_meta = dir.join(format!(".meta-{}-{n}", std::process::id()));
        let result = receive_file(&tmp, body, limits::DIST_BYTES, too_large, claimed).and_then(|size| {
            let meta = Meta {
                version: version.as_str().to_owned(),
                size,
                sha256: claimed.as_str().to_owned(),
                published_at: clock::now(),
            };
            let text = serde_json::to_vec(&meta).map_err(|e| ApiError::internal("encode the dist meta", &e))?;
            fs::write(&tmp_meta, text).map_err(|e| ApiError::internal("write the dist meta", &e))?;
            let _write = self.lock.write().unwrap_or_else(PoisonError::into_inner);
            fs::rename(&tmp, dir.join(target.exe_name()))
                .map_err(|e| ApiError::internal("store the rack binary", &e))?;
            // A crash between the two renames leaves a meta whose sha256 no longer matches the
            // file; clients verify it, refuse the download, and the next publish repairs it.
            fs::rename(&tmp_meta, dir.join(META)).map_err(|e| ApiError::internal("store the dist meta", &e))?;
            Ok(json!({
                "target": target.as_str(),
                "slot": slot.as_str(),
                "path": slot.url_path(target),
                "version": meta.version,
                "file": target.exe_name(),
                "size": meta.size,
                "sha256": meta.sha256,
            }))
        });
        if result.is_err() {
            for path in [&tmp, &tmp_meta] {
                if let Err(e) = fs::remove_file(path)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    tracing::warn!(target: "racking::dist", path = %path.display(), error = %e, "could not remove temp file");
                }
            }
        }
        result
    }

    /// `dist/<target>` predates the slots and stays the stable one, so data dirs
    /// written before them open unchanged.
    fn slot_dir(&self, slot: Slot, target: Target) -> PathBuf {
        match slot {
            Slot::Stable => self.dir.join(target.as_str()),
            Slot::Pre => self.dir.join("pre").join(target.as_str()),
        }
    }

    fn meta(&self, slot: Slot, target: Target) -> ApiResult<Option<Meta>> {
        let path = self.slot_dir(slot, target).join(META);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ApiError::internal("read the dist meta", &e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ApiError::internal("read the dist meta", &e)),
        }
    }
}

pub fn too_large() -> ApiError {
    ApiError::new(
        Code::TooLarge,
        "rack binary is over the limit",
        format!(
            "At most {} MiB per binary: build with --release.",
            limits::DIST_BYTES >> 20
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("racking-dist-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sha(bytes: &[u8]) -> Sha256Hex {
        Sha256Hex::of_digest(&Sha256::digest(bytes))
    }

    fn version(v: &str) -> Version {
        v.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn read(dist: &Dist, slot: Slot, target: Target) -> Vec<u8> {
        let mut dl = dist.download(slot, target).expect("download");
        let mut bytes = Vec::new();
        dl.file.read_to_end(&mut bytes).expect("read");
        bytes
    }

    fn files_in(dir: &Path) -> usize {
        fs::read_dir(dir).expect("dir").count()
    }

    #[test]
    fn publish_replaces_and_manifest_lists() {
        let dir = TempDir::new("replace");
        let dist = Dist::open(&dir.0).expect("open");
        let empty = json!({"version": null, "targets": {}});
        assert_eq!(
            dist.manifest().ok(),
            Some(json!({"version": null, "targets": {}, "prerelease": empty}))
        );
        assert_eq!(
            dist.download(Slot::Stable, Target::LinuxX86_64).err().map(|e| e.code),
            Some(Code::NotFound)
        );

        let err = dist
            .publish(Target::LinuxX86_64, &version("0.2.0"), &mut &b"v2"[..], &sha(b"other"))
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::ChecksumMismatch));

        dist.publish(Target::LinuxX86_64, &version("0.2.0"), &mut &b"v2"[..], &sha(b"v2"))
            .expect("publish");
        let published = dist
            .publish(Target::LinuxX86_64, &version("0.3.0"), &mut &b"v3!"[..], &sha(b"v3!"))
            .expect("publish again");
        assert_eq!(
            (published["slot"].as_str(), published["path"].as_str()),
            (Some("stable"), Some("/dist/x86_64-unknown-linux-gnu/rack"))
        );
        let m = dist.manifest().expect("manifest");
        assert_eq!(m["version"], "0.3.0");
        assert_eq!(
            m["targets"]["x86_64-unknown-linux-gnu"],
            json!({"version": "0.3.0", "file": "rack", "size": 3, "sha256": sha(b"v3!").as_str()})
        );
        assert_eq!(m["prerelease"], empty);
        assert_eq!(
            files_in(&dir.0.join("dist/x86_64-unknown-linux-gnu")),
            2,
            "only the binary and its meta, no temp files"
        );
        assert_eq!(read(&dist, Slot::Stable, Target::LinuxX86_64), b"v3!");
    }

    #[test]
    fn slots_are_kept_apart() {
        let dir = TempDir::new("slots");
        let dist = Dist::open(&dir.0).expect("open");
        let linux = Target::LinuxX86_64;
        dist.publish(linux, &version("0.1.3"), &mut &b"stable"[..], &sha(b"stable"))
            .expect("stable");
        let published = dist
            .publish(linux, &version("0.1.4-1"), &mut &b"test1"[..], &sha(b"test1"))
            .expect("pre");
        assert_eq!(
            (published["slot"].as_str(), published["path"].as_str()),
            (Some("pre"), Some("/dist/pre/x86_64-unknown-linux-gnu/rack"))
        );
        dist.publish(linux, &version("0.1.4-2"), &mut &b"test2"[..], &sha(b"test2"))
            .expect("pre again");

        let m = dist.manifest().expect("manifest");
        assert_eq!(
            (m["version"].as_str(), m["targets"][linux.as_str()]["version"].as_str()),
            (Some("0.1.3"), Some("0.1.3")),
            "a test build does not replace the stable release"
        );
        assert_eq!(
            m["prerelease"]["targets"][linux.as_str()],
            json!({"version": "0.1.4-2", "file": "rack", "size": 5, "sha256": sha(b"test2").as_str()})
        );
        assert_eq!(m["prerelease"]["version"], "0.1.4-2");
        assert_eq!(read(&dist, Slot::Stable, linux), b"stable");
        assert_eq!(read(&dist, Slot::Pre, linux), b"test2");
        assert_eq!(files_in(&dir.0.join("dist/pre/x86_64-unknown-linux-gnu")), 2);

        // A stable publish leaves the pre slot as it was; clients skip it once it is older.
        dist.publish(linux, &version("0.1.4"), &mut &b"final"[..], &sha(b"final"))
            .expect("stable 0.1.4");
        let m = dist.manifest().expect("manifest");
        assert_eq!(m["targets"][linux.as_str()]["version"], "0.1.4");
        assert_eq!(m["prerelease"]["targets"][linux.as_str()]["version"], "0.1.4-2");
        assert_eq!(read(&dist, Slot::Pre, linux), b"test2");
        assert_eq!(
            dist.download(Slot::Pre, Target::WindowsX86_64).err().map(|e| e.code),
            Some(Code::NotFound)
        );
    }

    #[test]
    fn opens_a_data_dir_from_before_the_slots() {
        // The layout and meta.json the 0.1.2 service writes.
        let dir = TempDir::new("old");
        let old = dir.0.join("dist/x86_64-pc-windows-gnu");
        fs::create_dir_all(&old).expect("old dir");
        fs::write(old.join("rack.exe"), b"MZ old").expect("old binary");
        let meta = json!({"version": "0.1.2", "size": 6, "sha256": sha(b"MZ old").as_str(), "published_at": 1});
        fs::write(old.join(META), meta.to_string()).expect("old meta");

        let dist = Dist::open(&dir.0).expect("open");
        let m = dist.manifest().expect("manifest");
        assert_eq!(m["version"], "0.1.2");
        assert_eq!(
            m["targets"]["x86_64-pc-windows-gnu"],
            json!({"version": "0.1.2", "file": "rack.exe", "size": 6, "sha256": sha(b"MZ old").as_str()})
        );
        assert_eq!(m["prerelease"], json!({"version": null, "targets": {}}));
        assert_eq!(read(&dist, Slot::Stable, Target::WindowsX86_64), b"MZ old");
    }

    #[test]
    fn size_limit() {
        let dir = TempDir::new("big");
        let dist = Dist::open(&dir.0).expect("open");
        for v in ["1.0.0", "1.0.0-1"] {
            let mut big = io::repeat(0).take(limits::DIST_BYTES + 1);
            let err = dist
                .publish(Target::WindowsX86_64, &version(v), &mut big, &sha(b""))
                .err()
                .map(|e| e.code);
            assert_eq!(err, Some(Code::TooLarge), "{v}");
        }
        assert!(!dir.0.join("dist/x86_64-pc-windows-gnu/rack.exe").exists());
        assert!(!dir.0.join("dist/pre/x86_64-pc-windows-gnu/rack.exe").exists());
    }
}
