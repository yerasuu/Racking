//! Local client state: cursors and the links from received jobs to local board
//! entries. Kept in one JSON file, written atomically.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::client::{RackError, Result, local, with_suffix};

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Cursor of `rack wait`.
    #[serde(default)]
    pub inbox_since: i64,
    /// Cursor of `rack outbox --wait`.
    #[serde(default)]
    pub outbox_since: i64,
    /// Every job up to this inbox `seq` was pulled; later ones may still need a retry.
    #[serde(default)]
    pub pull_since: i64,
    /// Job id to the local entry made (or commented) for it.
    #[serde(default)]
    pub links: BTreeMap<String, LocalLink>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LocalLink {
    pub repo: String,
    pub id: String,
    /// Whether `PUT /links` succeeded; an unsynced link is sent again on the next pull
    /// instead of creating a second entry.
    pub synced: bool,
}

impl State {
    /// A missing file is an empty state; a broken one is an error, so cursors
    /// and links are never silently reset.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                RackError::Local(format!(
                    "state file {} is not valid: {e}; fix or move it",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(local(&format!("read {}", path.display()), e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| local(&format!("create {}", dir.display()), e))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| local("encode state", e))?;
        let tmp = with_suffix(path, ".tmp");
        fs::write(&tmp, text).map_err(|e| local(&format!("write {}", tmp.display()), e))?;
        fs::rename(&tmp, path).map_err(|e| local(&format!("replace {}", path.display()), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("rack-state-{}", std::process::id()));
        let path = dir.join("state.json");
        let mut s = State::load(&path).expect("missing file is empty");
        s.pull_since = 3;
        s.links.insert(
            "j1".to_owned(),
            LocalLink {
                repo: "r".to_owned(),
                id: "31".to_owned(),
                synced: true,
            },
        );
        s.save(&path).expect("save");
        let back = State::load(&path).expect("load");
        assert_eq!(
            (back.pull_since, back.links.get("j1").map(|l| l.id.as_str())),
            (3, Some("31"))
        );
        fs::write(&path, "{nope").expect("write");
        assert!(State::load(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
