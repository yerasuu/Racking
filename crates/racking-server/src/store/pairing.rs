//! Pairing invites and the links between agents (#46). An invite keeps only the
//! salted SHA-256 of its code and, once redeemed, the SHA-256 of the token it
//! issued; neither the code nor the token is ever stored.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use super::{Parsed, Store};
use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::invite::{self, CONFIRM_SECS, MAX_FAILURES};
use crate::policy::Role;
use crate::types::AgentName;

/// New tables only, so `CREATE ... IF NOT EXISTS` is the whole migration. The
/// table of board entries is already called `links`, hence `agent_links`.
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS invites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    salt BLOB NOT NULL,
    code_sha256 BLOB NOT NULL,
    name TEXT NOT NULL,
    name_chosen INTEGER NOT NULL,
    role TEXT NOT NULL,
    inviter TEXT,
    state TEXT NOT NULL,
    failures INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    redeemed_at INTEGER,
    decided_at INTEGER,
    peer TEXT,
    os TEXT,
    rack_version TEXT,
    token_sha256 BLOB
) STRICT;
CREATE INDEX IF NOT EXISTS invites_token ON invites (token_sha256);
CREATE TABLE IF NOT EXISTS agent_links (
    agent_a TEXT NOT NULL,
    agent_b TEXT NOT NULL,
    state TEXT NOT NULL,
    origin TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    confirmed_at INTEGER,
    PRIMARY KEY (agent_a, agent_b)
) STRICT;
";

/// Where an invite stands. `Expired` is never stored: it is an `open` invite past its `expires_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteState {
    Open,
    Expired,
    Redeemed,
    Confirmed,
    Denied,
    Voided,
}

impl InviteState {
    pub fn as_str(self) -> &'static str {
        match self {
            InviteState::Open => "open",
            InviteState::Expired => "expired",
            InviteState::Redeemed => "redeemed",
            InviteState::Confirmed => "confirmed",
            InviteState::Denied => "denied",
            InviteState::Voided => "voided",
        }
    }

    fn stored(text: &str, expires_at: i64, now: i64) -> rusqlite::Result<Self> {
        Ok(match text {
            "open" if expires_at <= now => InviteState::Expired,
            "open" => InviteState::Open,
            "redeemed" => InviteState::Redeemed,
            "confirmed" => InviteState::Confirmed,
            "denied" => InviteState::Denied,
            "voided" => InviteState::Voided,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("invite state `{other}`").into(),
                ));
            }
        })
    }
}

/// An invite as stored, without its code hash.
pub struct Invite {
    pub id: i64,
    pub name: AgentName,
    /// The admin picked the name; a joiner may not change it.
    pub name_chosen: bool,
    pub role: Role,
    /// The admin agent that made it; none for one made with `racking-server invite`.
    pub inviter: Option<AgentName>,
    pub state: InviteState,
    pub failures: u32,
    pub created_at: i64,
    pub expires_at: i64,
    pub redeemed_at: Option<i64>,
    pub peer: Option<String>,
    pub os: Option<String>,
    pub rack_version: Option<String>,
    pub token_digest: Option<[u8; 32]>,
}

impl Invite {
    pub fn fingerprint(&self) -> Option<String> {
        self.token_digest
            .as_ref()
            .map(|d| invite::fingerprint(self.id, &self.name, d))
    }

    /// The time the inviter has left to confirm, once redeemed.
    pub fn confirm_by(&self) -> Option<i64> {
        self.redeemed_at.map(|at| at + CONFIRM_SECS)
    }

    /// Everything but the code and token hashes.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name.as_str(),
            "role": self.role.as_str(),
            "inviter": self.inviter.as_ref().map(AgentName::as_str),
            "state": self.state.as_str(),
            "failures": self.failures,
            "created_at": clock::rfc3339(self.created_at),
            "expires_at": clock::rfc3339(self.expires_at),
            "redeemed_at": self.redeemed_at.map(clock::rfc3339),
            "confirm_by": self.confirm_by().map(clock::rfc3339),
            "peer": self.peer,
            "os": self.os,
            "rack_version": self.rack_version,
            "fingerprint": self.fingerprint(),
        })
    }
}

const INVITE_COLUMNS: &str = "id, name, name_chosen, role, inviter, state, failures, created_at, expires_at, \
                              redeemed_at, peer, os, rack_version, token_sha256";

fn invite_from_row(row: &Row<'_>, now: i64) -> rusqlite::Result<Invite> {
    let expires_at = row.get(8)?;
    Ok(Invite {
        id: row.get(0)?,
        name: row.get::<_, Parsed<AgentName>>(1)?.0,
        name_chosen: row.get(2)?,
        role: row.get::<_, Parsed<Role>>(3)?.0,
        inviter: row.get::<_, Option<Parsed<AgentName>>>(4)?.map(|p| p.0),
        state: InviteState::stored(&row.get::<_, String>(5)?, expires_at, now)?,
        failures: row.get(6)?,
        created_at: row.get(7)?,
        expires_at,
        redeemed_at: row.get(9)?,
        peer: row.get(10)?,
        os: row.get(11)?,
        rack_version: row.get(12)?,
        token_digest: row.get(13)?,
    })
}

/// What an admin asks for when it makes an invite, already checked.
pub struct NewInvite<'a> {
    pub salt: &'a [u8],
    pub code_hash: &'a [u8; 32],
    pub name: &'a AgentName,
    pub name_chosen: bool,
    pub role: Role,
    pub inviter: Option<&'a AgentName>,
    pub expires_at: i64,
}

/// What a joiner presents with a valid code, already checked.
pub struct Redeem<'a> {
    pub invite: i64,
    pub name: &'a AgentName,
    pub role: Role,
    pub token_digest: &'a [u8; 32],
    pub peer: &'a str,
    pub os: Option<&'a str>,
    pub rack_version: Option<&'a str>,
}

/// What a presented code is checked against.
pub struct LiveCode {
    pub id: i64,
    pub salt: Vec<u8>,
    pub hash: [u8; 32],
}

/// One stored link, from the point of view of one of its agents.
pub struct LinkRow {
    pub with: AgentName,
    pub state: String,
    pub origin: String,
    pub created_at: i64,
    pub confirmed_at: Option<i64>,
}

/// The inviter's answer after comparing the fingerprints.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Confirm,
    Deny,
}

fn ordered<'a>(a: &'a AgentName, b: &'a AgentName) -> (&'a AgentName, &'a AgentName) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The bad-code answer, the same whether the code is unknown, used, expired or voided,
/// so a guesser learns nothing from it.
pub fn bad_code() -> ApiError {
    ApiError::new(
        Code::InviteRefused,
        "this code is not valid",
        "Check it with the person who ran `rack link`: codes are single use and expire. Ask for a new one if \
         in doubt.",
    )
}

impl Store {
    pub fn create_invite(&self, new: &NewInvite<'_>, now: i64) -> ApiResult<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO invites (salt, code_sha256, name, name_chosen, role, inviter, state, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8)",
            params![
                new.salt,
                new.code_hash,
                new.name.as_str(),
                new.name_chosen,
                new.role.as_str(),
                new.inviter.map(AgentName::as_str),
                now,
                new.expires_at
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn invite(&self, id: i64, now: i64) -> ApiResult<Option<Invite>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE id = ?1"),
                [id],
                |row| invite_from_row(row, now),
            )
            .optional()?)
    }

    /// Invites still waiting for a joiner or for their inviter's confirmation.
    pub fn pending_invites(&self, now: i64) -> ApiResult<Vec<Invite>> {
        let conn = self.db();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {INVITE_COLUMNS} FROM invites WHERE (state = 'open' AND expires_at > ?1) OR state = 'redeemed' \
             ORDER BY id"
        ))?;
        let rows = stmt.query_map([now], |row| invite_from_row(row, now))?;
        rows.map(|r| r.map_err(ApiError::from)).collect()
    }

    /// The salt and code hash of every invite a code could still redeem.
    pub fn live_codes(&self, now: i64) -> ApiResult<Vec<LiveCode>> {
        let conn = self.db();
        let mut stmt =
            conn.prepare_cached("SELECT id, salt, code_sha256 FROM invites WHERE state = 'open' AND expires_at > ?1")?;
        let rows = stmt.query_map([now], |row| {
            Ok(LiveCode {
                id: row.get(0)?,
                salt: row.get(1)?,
                hash: row.get(2)?,
            })
        })?;
        rows.map(|r| r.map_err(ApiError::from)).collect()
    }

    /// Counts a wrong code against every live invite, since a code names no invite
    /// until it matches; voids those that reached `MAX_FAILURES`. Returns their ids.
    pub fn count_bad_code(&self, now: i64) -> ApiResult<Vec<i64>> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE invites SET failures = failures + 1 WHERE state = 'open' AND expires_at > ?1",
            [now],
        )?;
        let voided = {
            let mut stmt = tx.prepare(
                "UPDATE invites SET state = 'voided', decided_at = ?1 WHERE state = 'open' AND failures >= ?2 \
                 RETURNING id",
            )?;
            let rows = stmt.query_map(params![now, MAX_FAILURES], |row| row.get(0))?;
            rows.collect::<Result<Vec<i64>, _>>()?
        };
        tx.commit()?;
        Ok(voided)
    }

    /// Whether a name is taken by an invite that may still become that agent.
    pub fn name_reserved(&self, name: &AgentName, now: i64) -> ApiResult<bool> {
        Ok(self.db().query_row(
            "SELECT EXISTS (SELECT 1 FROM invites WHERE name = ?1 AND \
             ((state = 'open' AND expires_at > ?2) OR state = 'redeemed'))",
            params![name.as_str(), now],
            |row| row.get(0),
        )?)
    }

    /// Whether an invite that may still become an agent asks for the main role.
    pub fn main_invited(&self, now: i64) -> ApiResult<bool> {
        Ok(self.db().query_row(
            "SELECT EXISTS (SELECT 1 FROM invites WHERE role = 'main' AND \
             ((state = 'open' AND expires_at > ?1) OR state = 'redeemed'))",
            [now],
            |row| row.get(0),
        )?)
    }

    /// Marks the invite used and stores the new agent and its pending link in one
    /// transaction, so a code redeemed twice at once makes one agent: the second
    /// finds the invite no longer open and gets the bad-code answer.
    pub fn redeem(&self, r: &Redeem<'_>, now: i64) -> ApiResult<()> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let inviter: Option<Option<String>> = tx
            .query_row(
                "UPDATE invites SET state = 'redeemed', name = ?2, redeemed_at = ?3, peer = ?4, os = ?5, \
                 rack_version = ?6, token_sha256 = ?7 WHERE id = ?1 AND state = 'open' AND expires_at > ?3 \
                 RETURNING inviter",
                params![
                    r.invite,
                    r.name.as_str(),
                    now,
                    r.peer,
                    r.os,
                    r.rack_version,
                    r.token_digest
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(inviter) = inviter else {
            return Err(bad_code());
        };
        tx.execute(
            "INSERT INTO managed_agents (name, role, token_sha256, created_at, token_created_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![r.name.as_str(), r.role.as_str(), r.token_digest, now],
        )?;
        if let Some(inviter) = inviter {
            let inviter: AgentName = inviter
                .parse()
                .map_err(|e: String| ApiError::internal("read an invite", &e))?;
            let (a, b) = ordered(&inviter, r.name);
            tx.execute(
                "INSERT INTO agent_links (agent_a, agent_b, state, origin, created_at) \
                 VALUES (?1, ?2, 'pending', 'invite', ?3) \
                 ON CONFLICT (agent_a, agent_b) DO UPDATE SET state = 'pending', origin = 'invite', \
                 created_at = ?3, confirmed_at = NULL",
                params![a.as_str(), b.as_str(), now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Confirms or denies a redeemed invite within `CONFIRM_SECS` of its redemption.
    /// Denying deletes the pending agent and its link from the store; the caller
    /// drops them from memory. Returns the invite as it ends.
    pub fn decide(&self, id: i64, decision: Decision, now: i64) -> ApiResult<Invite> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let invite = tx
            .query_row(
                &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE id = ?1"),
                [id],
                |row| invite_from_row(row, now),
            )
            .optional()?
            .ok_or_else(|| no_invite(id))?;
        if invite.state != InviteState::Redeemed {
            return Err(ApiError::new(
                Code::Conflict,
                format!(
                    "invite {id} is {}, not waiting for a confirmation",
                    invite.state.as_str()
                ),
                "Only an invite whose code was redeemed can be confirmed or denied; GET /admin/invites lists them.",
            ));
        }
        let late = invite.confirm_by().is_some_and(|by| by <= now);
        let decision = if late { Decision::Deny } else { decision };
        settle(&tx, &invite, decision, now)?;
        tx.commit()?;
        if late {
            return Err(ApiError::new(
                Code::Conflict,
                format!("invite {id} waited more than {CONFIRM_SECS} s for a confirmation and was denied"),
                "Make a new invite and compare the words sooner.",
            ));
        }
        Ok(Invite {
            state: match decision {
                Decision::Confirm => InviteState::Confirmed,
                Decision::Deny => InviteState::Denied,
            },
            ..invite
        })
    }

    /// Denies every redeemed invite whose confirmation time ran out; returns the agents removed.
    pub fn deny_unconfirmed(&self, now: i64) -> ApiResult<Vec<AgentName>> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let late = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {INVITE_COLUMNS} FROM invites WHERE state = 'redeemed' AND redeemed_at <= ?1"
            ))?;
            let rows = stmt.query_map([now - CONFIRM_SECS], |row| invite_from_row(row, now))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for invite in &late {
            settle(&tx, invite, Decision::Deny, now)?;
        }
        tx.commit()?;
        Ok(late.into_iter().map(|i| i.name).collect())
    }

    /// Voids an invite whose code was not redeemed yet.
    pub fn void_invite(&self, id: i64, now: i64) -> ApiResult<Invite> {
        let invite = self.invite(id, now)?.ok_or_else(|| no_invite(id))?;
        if invite.state != InviteState::Open {
            return Err(ApiError::new(
                Code::Conflict,
                format!("invite {id} is {}", invite.state.as_str()),
                "Only an open invite can be voided; deny a redeemed one with POST /admin/invites/{id}/deny.",
            ));
        }
        self.db().execute(
            "UPDATE invites SET state = 'voided', decided_at = ?2 WHERE id = ?1 AND state = 'open'",
            params![id, now],
        )?;
        Ok(Invite {
            state: InviteState::Voided,
            ..invite
        })
    }

    /// The invite that issued a token, by the token's SHA-256: how `GET /join/status`
    /// answers a joiner whose agent may already be denied and gone.
    pub fn invite_by_token(&self, digest: &[u8; 32], now: i64) -> ApiResult<Option<Invite>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE token_sha256 = ?1 ORDER BY id DESC LIMIT 1"),
                [digest],
                |row| invite_from_row(row, now),
            )
            .optional()?)
    }

    /// The last invite redeemed as `name`, whatever became of it.
    pub fn invite_of(&self, name: &AgentName, now: i64) -> ApiResult<Option<Invite>> {
        Ok(self
            .db()
            .query_row(
                &format!(
                    "SELECT {INVITE_COLUMNS} FROM invites WHERE name = ?1 AND token_sha256 IS NOT NULL \
                     ORDER BY id DESC LIMIT 1"
                ),
                [name.as_str()],
                |row| invite_from_row(row, now),
            )
            .optional()?)
    }

    pub fn confirmed_links(&self) -> Result<Vec<(AgentName, AgentName)>, String> {
        let conn = self.db();
        let mut stmt = conn
            .prepare("SELECT agent_a, agent_b FROM agent_links WHERE state = 'confirmed'")
            .map_err(|e| format!("cannot read the agent_links table: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Parsed<AgentName>>(0)?.0,
                    row.get::<_, Parsed<AgentName>>(1)?.0,
                ))
            })
            .map_err(|e| format!("cannot read the agent_links table: {e}"))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| format!("cannot read the agent_links table: {e}"))
    }

    pub fn links_of(&self, name: &AgentName) -> ApiResult<Vec<LinkRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare_cached(
            "SELECT CASE WHEN agent_a = ?1 THEN agent_b ELSE agent_a END, state, origin, created_at, confirmed_at \
             FROM agent_links WHERE agent_a = ?1 OR agent_b = ?1 ORDER BY 1",
        )?;
        let rows = stmt.query_map([name.as_str()], |row| {
            Ok(LinkRow {
                with: row.get::<_, Parsed<AgentName>>(0)?.0,
                state: row.get(1)?,
                origin: row.get(2)?,
                created_at: row.get(3)?,
                confirmed_at: row.get(4)?,
            })
        })?;
        rows.map(|r| r.map_err(ApiError::from)).collect()
    }

    /// A confirmed link made by an admin; an existing one is confirmed again.
    pub fn add_link(&self, a: &AgentName, b: &AgentName, now: i64) -> ApiResult<()> {
        let (a, b) = ordered(a, b);
        self.db().execute(
            "INSERT INTO agent_links (agent_a, agent_b, state, origin, created_at, confirmed_at) \
             VALUES (?1, ?2, 'confirmed', 'admin', ?3, ?3) \
             ON CONFLICT (agent_a, agent_b) DO UPDATE SET state = 'confirmed', confirmed_at = ?3",
            params![a.as_str(), b.as_str(), now],
        )?;
        Ok(())
    }

    /// Removes a link; returns its origin, or `None` when there was none.
    pub fn delete_link(&self, a: &AgentName, b: &AgentName) -> ApiResult<Option<String>> {
        let (a, b) = ordered(a, b);
        Ok(self
            .db()
            .query_row(
                "DELETE FROM agent_links WHERE agent_a = ?1 AND agent_b = ?2 RETURNING origin",
                params![a.as_str(), b.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }
}

/// Applies a decision inside the caller's transaction.
fn settle(conn: &Connection, invite: &Invite, decision: Decision, now: i64) -> rusqlite::Result<()> {
    let name = invite.name.as_str();
    match decision {
        Decision::Confirm => {
            conn.execute(
                "UPDATE invites SET state = 'confirmed', decided_at = ?2 WHERE id = ?1",
                params![invite.id, now],
            )?;
            if let Some(inviter) = &invite.inviter {
                let (a, b) = ordered(inviter, &invite.name);
                conn.execute(
                    "UPDATE agent_links SET state = 'confirmed', confirmed_at = ?3 WHERE agent_a = ?1 AND agent_b = ?2",
                    params![a.as_str(), b.as_str(), now],
                )?;
            }
        }
        Decision::Deny => {
            conn.execute(
                "UPDATE invites SET state = 'denied', decided_at = ?2 WHERE id = ?1",
                params![invite.id, now],
            )?;
            conn.execute("DELETE FROM managed_agents WHERE name = ?1", [name])?;
            conn.execute("DELETE FROM agent_links WHERE agent_a = ?1 OR agent_b = ?1", [name])?;
        }
    }
    Ok(())
}

fn no_invite(id: i64) -> ApiError {
    ApiError::new(
        Code::NotFound,
        format!("no invite {id}"),
        "GET /admin/invites lists the pending ones.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invite::InviteCode;

    struct Dir(std::path::PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(tag: &str) -> (Dir, Store) {
        let dir = std::env::temp_dir().join(format!("racking-pairing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("store");
        (Dir(dir), store)
    }

    fn name(n: &str) -> AgentName {
        n.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn invite(s: &Store, who: &AgentName, now: i64) -> (i64, InviteCode) {
        let code = InviteCode::generate().expect("code");
        let salt = [5u8; 16];
        let id = s
            .create_invite(
                &NewInvite {
                    salt: &salt,
                    code_hash: &code.hash(&salt),
                    name: who,
                    name_chosen: true,
                    role: Role::Worker,
                    inviter: Some(&name("boss")),
                    expires_at: now + 600,
                },
                now,
            )
            .expect("invite");
        (id, code)
    }

    fn redeem(s: &Store, id: i64, who: &AgentName, now: i64) -> ApiResult<()> {
        s.redeem(
            &Redeem {
                invite: id,
                name: who,
                role: Role::Worker,
                token_digest: &[9u8; 32],
                peer: "192.168.1.5",
                os: Some("linux"),
                rack_version: Some("0.1.3"),
            },
            now,
        )
    }

    #[test]
    fn invites_expire_and_count_failures() {
        let (_d, s) = store("expire");
        let (id, code) = invite(&s, &name("mac"), 1000);
        let live = s.live_codes(1000).expect("live");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].hash, code.hash(&live[0].salt), "only the salted hash is kept");
        assert!(s.name_reserved(&name("mac"), 1000).expect("reserved"));
        assert_eq!(
            s.invite(id, 1600).expect("read").map(|i| i.state),
            Some(InviteState::Expired)
        );
        assert!(s.live_codes(1600).expect("live").is_empty());
        assert!(!s.name_reserved(&name("mac"), 1600).expect("free again"));

        let (other, _) = invite(&s, &name("pc"), 1000);
        for _ in 0..MAX_FAILURES - 1 {
            assert!(s.count_bad_code(1100).expect("count").is_empty());
        }
        assert_eq!(s.count_bad_code(1100).expect("count"), vec![id, other]);
        assert_eq!(
            s.invite(other, 1100).expect("read").map(|i| (i.state, i.failures)),
            Some((InviteState::Voided, MAX_FAILURES))
        );
        assert_eq!(
            redeem(&s, other, &name("pc"), 1100).err().map(|e| e.code),
            Some(Code::InviteRefused)
        );
    }

    #[test]
    fn redeem_once_then_confirm_or_deny() {
        let (_d, s) = store("decide");
        let (mac, pc) = (name("mac"), name("pc"));
        let (id, _) = invite(&s, &mac, 1000);
        redeem(&s, id, &mac, 1010).expect("redeem");
        assert_eq!(
            redeem(&s, id, &mac, 1011).err().map(|e| e.code),
            Some(Code::InviteRefused),
            "single use"
        );
        let rows = s.managed_agents().expect("rows");
        assert!(rows.iter().any(|r| r.name == "mac" && r.pending));
        assert!(
            s.confirmed_links().expect("links").is_empty(),
            "pending until confirmed"
        );
        let read = s.invite(id, 1010).expect("read").expect("invite");
        assert_eq!(read.fingerprint(), Some(invite::fingerprint(id, &mac, &[9u8; 32])));
        assert!(s.invite_by_token(&[9u8; 32], 1010).expect("by token").is_some());

        s.decide(id, Decision::Confirm, 1020).expect("confirm");
        assert_eq!(s.confirmed_links().expect("links"), vec![(name("boss"), mac.clone())]);
        assert!(!s.managed_agents().expect("rows").iter().any(|r| r.pending));
        assert_eq!(
            s.decide(id, Decision::Deny, 1030).err().map(|e| e.code),
            Some(Code::Conflict),
            "decided once"
        );

        let (id, _) = invite(&s, &pc, 1000);
        redeem(&s, id, &pc, 1010).expect("redeem");
        s.decide(id, Decision::Deny, 1020).expect("deny");
        assert!(!s.managed_agents().expect("rows").iter().any(|r| r.name == "pc"));
        assert!(s.links_of(&pc).expect("links").is_empty());
        assert_eq!(
            s.invite_of(&pc, 1020).expect("read").map(|i| i.state),
            Some(InviteState::Denied)
        );
    }

    #[test]
    fn late_confirmations_are_denials() {
        let (_d, s) = store("late");
        let mac = name("mac");
        let (id, _) = invite(&s, &mac, 1000);
        redeem(&s, id, &mac, 1000).expect("redeem");
        assert!(
            s.deny_unconfirmed(1000 + CONFIRM_SECS - 1)
                .expect("none yet")
                .is_empty()
        );
        assert_eq!(
            s.decide(id, Decision::Confirm, 1000 + CONFIRM_SECS)
                .err()
                .map(|e| e.code),
            Some(Code::Conflict)
        );
        assert_eq!(
            s.invite(id, 1000 + CONFIRM_SECS).expect("read").map(|i| i.state),
            Some(InviteState::Denied)
        );
        assert!(s.managed_agents().expect("rows").is_empty());
    }

    #[test]
    fn admin_links_and_their_removal() {
        let (_d, s) = store("links");
        let (w1, w2) = (name("w1"), name("w2"));
        s.add_link(&w2, &w1, 5).expect("link");
        assert_eq!(s.confirmed_links().expect("links"), vec![(w1.clone(), w2.clone())]);
        assert_eq!(
            s.links_of(&w1).expect("of").first().map(|l| l.with.clone()),
            Some(w2.clone())
        );
        assert_eq!(s.delete_link(&w1, &w2).expect("delete").as_deref(), Some("admin"));
        assert_eq!(s.delete_link(&w1, &w2).expect("again"), None);
    }

    #[test]
    fn old_databases_gain_the_pairing_tables() {
        let dir = std::env::temp_dir().join(format!("racking-pairing-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let _guard = Dir(dir.clone());
        let conn = Connection::open(dir.join("racking.db")).expect("db");
        conn.execute_batch(
            "CREATE TABLE managed_agents (name TEXT PRIMARY KEY, role TEXT NOT NULL, token_sha256 BLOB NOT NULL, \
             created_at INTEGER NOT NULL, token_created_at INTEGER NOT NULL) STRICT;
             INSERT INTO managed_agents VALUES ('mac', 'worker', zeroblob(32), 1, 1);",
        )
        .expect("schema before pairing");
        drop(conn);
        let s = Store::open(&dir).expect("open");
        let rows = s.managed_agents().expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].pending, "an agent made before invites is active");
        assert!(s.confirmed_links().expect("links").is_empty());
        drop(s);
        Store::open(&dir).expect("opening twice is fine");
    }
}
