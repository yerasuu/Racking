//! `rack` binaries served for `rack update` (decision #21): the service is the
//! update origin on the LAN. One file per target, the latest one published,
//! under `<data_dir>/dist/<target>/` next to a `meta.json` with its version,
//! size and SHA-256. The SHA-256 is the only integrity check: nothing is signed.

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

    /// `{"version", "targets": {<triple>: {"version", "file", "size", "sha256"}}}`; the
    /// top `version` is the one published last, null while nothing is.
    pub fn manifest(&self) -> ApiResult<Value> {
        let _read = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        let mut targets = Map::new();
        let mut newest: Option<(i64, String)> = None;
        for target in Target::ALL {
            let Some(meta) = self.meta(target)? else {
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

    pub fn download(&self, target: Target) -> ApiResult<Download> {
        let _read = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        let meta = self.meta(target)?.ok_or_else(|| {
            ApiError::new(
                Code::NotFound,
                format!("no rack published for {target}"),
                "GET /dist/manifest lists the published targets; an admin agent publishes with `rack publish`.",
            )
        })?;
        let file = File::open(self.file_path(target)).map_err(|e| ApiError::internal("open the rack binary", &e))?;
        Ok(Download {
            file,
            sha256: meta.sha256,
        })
    }

    /// Streams the upload next to its final place, checks size and hash, then
    /// replaces the target's binary and meta. The caller has checked the agent may publish.
    pub fn publish(
        &self,
        target: Target,
        version: &Version,
        body: &mut dyn Read,
        claimed: &Sha256Hex,
    ) -> ApiResult<Value> {
        let dir = self.dir.join(target.as_str());
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
            fs::rename(&tmp, self.file_path(target)).map_err(|e| ApiError::internal("store the rack binary", &e))?;
            // A crash between the two renames leaves a meta whose sha256 no longer matches the
            // file; clients verify it, refuse the download, and the next publish repairs it.
            fs::rename(&tmp_meta, dir.join(META)).map_err(|e| ApiError::internal("store the dist meta", &e))?;
            Ok(json!({
                "target": target.as_str(),
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

    fn file_path(&self, target: Target) -> PathBuf {
        self.dir.join(target.as_str()).join(target.exe_name())
    }

    fn meta(&self, target: Target) -> ApiResult<Option<Meta>> {
        let path = self.dir.join(target.as_str()).join(META);
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

    #[test]
    fn publish_replaces_and_manifest_lists() {
        let dir = TempDir(std::env::temp_dir().join(format!("racking-dist-{}", std::process::id())));
        let _ = fs::remove_dir_all(&dir.0);
        let dist = Dist::open(&dir.0).expect("open");
        assert_eq!(dist.manifest().ok(), Some(json!({"version": null, "targets": {}})));
        assert_eq!(
            dist.download(Target::LinuxX86_64).err().map(|e| e.code),
            Some(Code::NotFound)
        );

        let err = dist
            .publish(Target::LinuxX86_64, &version("0.2.0"), &mut &b"v2"[..], &sha(b"other"))
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::ChecksumMismatch));

        dist.publish(Target::LinuxX86_64, &version("0.2.0"), &mut &b"v2"[..], &sha(b"v2"))
            .expect("publish");
        dist.publish(Target::LinuxX86_64, &version("0.3.0"), &mut &b"v3!"[..], &sha(b"v3!"))
            .expect("publish again");
        let m = dist.manifest().expect("manifest");
        assert_eq!(m["version"], "0.3.0");
        assert_eq!(
            m["targets"]["x86_64-unknown-linux-gnu"],
            json!({"version": "0.3.0", "file": "rack", "size": 3, "sha256": sha(b"v3!").as_str()})
        );
        let names: Vec<_> = fs::read_dir(dir.0.join("dist/x86_64-unknown-linux-gnu"))
            .expect("dir")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(names.len(), 2, "only the binary and its meta, no temp files: {names:?}");

        let mut dl = dist.download(Target::LinuxX86_64).expect("download");
        let mut bytes = Vec::new();
        dl.file.read_to_end(&mut bytes).expect("read");
        assert_eq!(
            (bytes.as_slice(), dl.sha256.as_str()),
            (&b"v3!"[..], sha(b"v3!").as_str())
        );
    }

    #[test]
    fn size_limit() {
        let dir = TempDir(std::env::temp_dir().join(format!("racking-dist-big-{}", std::process::id())));
        let dist = Dist::open(&dir.0).expect("open");
        let mut big = io::repeat(0).take(limits::DIST_BYTES + 1);
        let err = dist
            .publish(Target::WindowsX86_64, &version("1.0.0"), &mut big, &sha(b""))
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(Code::TooLarge));
        assert!(!dir.0.join("dist/x86_64-pc-windows-gnu/rack.exe").exists());
    }
}
