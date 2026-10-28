//! Agents made through the admin API, the audit log and the pause flag. Tokens
//! are never stored: only their SHA-256, so a copy of the database grants nothing.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::Store;
use crate::clock;
use crate::error::ApiResult;
use crate::policy::Role;
use crate::types::AgentName;

/// New tables only, so `CREATE ... IF NOT EXISTS` is the whole migration.
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS managed_agents (
    name TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    token_sha256 BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    token_created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    result TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS settings (name TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
";

/// An agent the admin API created, as stored.
pub struct Managed {
    pub name: String,
    pub role: String,
    pub digest: [u8; 32],
    pub token_created_at: i64,
    /// Joined with a code and still waiting for its inviter's confirmation.
    pub pending: bool,
}

/// One audit row: who did what to whom, and how it ended. Never a body, a code or a token.
pub struct AuditEntry<'a> {
    /// An agent name; the peer address for a failed join, `owner` for `racking-server invite`.
    pub actor: &'a str,
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub result: &'a str,
}

impl Store {
    pub fn managed_agents(&self) -> Result<Vec<Managed>, String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare(
                "SELECT name, role, token_sha256, token_created_at, EXISTS (SELECT 1 FROM invites i \
                 WHERE i.name = managed_agents.name AND i.state = 'redeemed') \
                 FROM managed_agents ORDER BY created_at, name",
            )
            .map_err(|e| format!("cannot read the managed_agents table: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Managed {
                    name: row.get(0)?,
                    role: row.get(1)?,
                    digest: row.get(2)?,
                    token_created_at: row.get(3)?,
                    pending: row.get(4)?,
                })
            })
            .map_err(|e| format!("cannot read the managed_agents table: {e}"))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| format!("cannot read the managed_agents table: {e}"))
    }

    pub fn insert_managed(&self, name: &AgentName, role: Role, digest: &[u8; 32], now: i64) -> ApiResult<()> {
        self.db().execute(
            "INSERT INTO managed_agents (name, role, token_sha256, created_at, token_created_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![name.as_str(), role.as_str(), digest, now],
        )?;
        Ok(())
    }

    pub fn set_managed_token(&self, name: &AgentName, digest: &[u8; 32], now: i64) -> ApiResult<()> {
        self.db().execute(
            "UPDATE managed_agents SET token_sha256 = ?2, token_created_at = ?3 WHERE name = ?1",
            params![name.as_str(), digest, now],
        )?;
        Ok(())
    }

    /// Forgets the agent, its token and its links, so a later agent of the same name
    /// inherits none; its jobs stay, as the protocol keeps them for both parties.
    pub fn delete_managed(&self, name: &AgentName) -> ApiResult<()> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM managed_agents WHERE name = ?1", [name.as_str()])?;
        tx.execute(
            "DELETE FROM agent_links WHERE agent_a = ?1 OR agent_b = ?1",
            [name.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn audit(&self, entry: &AuditEntry<'_>) -> ApiResult<()> {
        self.db().execute(
            "INSERT INTO audit (at, actor, action, target, result) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![clock::now(), entry.actor, entry.action, entry.target, entry.result],
        )?;
        Ok(())
    }

    /// Audit rows newest first, those with `id < before` when given. The answer's
    /// `next` is the `before` of the following page, null on the last one.
    pub fn audit_page(&self, before: Option<i64>, limit: u32) -> ApiResult<Value> {
        let conn = self.db();
        let mut stmt = conn.prepare_cached(
            "SELECT id, at, actor, action, target, result FROM audit WHERE (?1 IS NULL OR id < ?1) \
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![before, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                json!({
                    "id": row.get::<_, i64>(0)?,
                    "at": clock::rfc3339(row.get(1)?),
                    "actor": row.get::<_, String>(2)?,
                    "action": row.get::<_, String>(3)?,
                    "target": row.get::<_, Option<String>>(4)?,
                    "result": row.get::<_, String>(5)?,
                }),
            ))
        })?;
        let mut entries = Vec::with_capacity(limit as usize);
        let mut last = None;
        for row in rows {
            let (id, entry) = row?;
            last = Some(id);
            entries.push(entry);
        }
        let next = last.filter(|_| entries.len() == limit as usize);
        Ok(json!({"entries": entries, "next": next}))
    }

    pub fn paused(&self) -> Result<bool, String> {
        read_paused(&self.db()).map_err(|e| format!("cannot read the pause flag: {e}"))
    }

    pub fn set_paused(&self, paused: bool) -> ApiResult<()> {
        self.db().execute(
            "INSERT INTO settings (name, value) VALUES ('paused', ?1) \
             ON CONFLICT (name) DO UPDATE SET value = ?1",
            [if paused { "1" } else { "0" }],
        )?;
        Ok(())
    }
}

fn read_paused(conn: &Connection) -> rusqlite::Result<bool> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE name = 'paused'", [], |row| row.get(0))
        .optional()?;
    Ok(value.as_deref() == Some("1"))
}
