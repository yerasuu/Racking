//! Making invites, shared by `POST /admin/invites` and `racking-server invite`,
//! and the owner's bootstrap: an invite made on the server machine itself, with
//! the database opened directly instead of an admin token.

use crate::accounts::main_needs_roles;
use crate::clock;
use crate::config::Config;
use crate::error::{ApiError, ApiResult, Code};
use crate::invite::{self, DEFAULT_TTL_SECS, InviteCode};
use crate::policy::Role;
use crate::store::{AuditEntry, Decision, InviteState, NewInvite, Store};
use crate::types::AgentName;

/// The agents an invite must not clash with.
pub struct Roster {
    pub names: Vec<AgentName>,
    pub main: Option<AgentName>,
    pub roles_enforced: bool,
}

/// An invite just made. `code` is shown once, to whoever asked for it.
pub struct Made {
    pub id: i64,
    pub code: InviteCode,
    pub name: AgentName,
    pub role: Role,
    pub expires_at: i64,
}

/// Checks the name and role rules of decision #44 and stores a new invite.
pub fn make_invite(
    store: &Store,
    roster: &Roster,
    inviter: Option<&AgentName>,
    name: Option<AgentName>,
    role: Role,
    ttl: i64,
    now: i64,
) -> ApiResult<Made> {
    if role == Role::Main {
        if !roster.roles_enforced {
            return Err(main_needs_roles());
        }
        if let Some(main) = &roster.main {
            return Err(ApiError::new(
                Code::Conflict,
                format!("{main} is main already"),
                "Only one agent may be main; invite this one as a worker.",
            ));
        }
        if store.main_invited(now)? {
            return Err(ApiError::new(
                Code::Conflict,
                "an open invite asks for the main role already",
                "Only one agent may be main: cancel that invite first (DELETE /admin/invites/{id}).",
            ));
        }
    }
    let taken = |n: &AgentName| -> ApiResult<bool> { Ok(roster.names.contains(n) || store.name_reserved(n, now)?) };
    let (name, chosen) = match name {
        Some(n) => {
            if taken(&n)? {
                return Err(ApiError::new(
                    Code::Conflict,
                    format!("the name {n} is taken by an agent or an open invite"),
                    "Pick another name, or leave it out and the server proposes one.",
                ));
            }
            (n, true)
        }
        None => {
            let mut proposed = None;
            // 31^4 names, so a clash twice in a row is already unlikely; the bound only stops a full roster.
            for _ in 0..16 {
                let n = invite::proposed_name()?;
                if !taken(&n)? {
                    proposed = Some(n);
                    break;
                }
            }
            let n = proposed.ok_or_else(|| {
                ApiError::new(
                    Code::Conflict,
                    "no free agent name could be proposed",
                    "Give the invite a name.",
                )
            })?;
            (n, false)
        }
    };
    let code = InviteCode::generate()?;
    let salt = invite::new_salt()?;
    let expires_at = now + ttl;
    let id = store.create_invite(
        &NewInvite {
            salt: &salt,
            code_hash: &code.hash(&salt),
            name: &name,
            name_chosen: chosen,
            role,
            inviter,
            expires_at,
        },
        now,
    )?;
    Ok(Made {
        id,
        code,
        name,
        role,
        expires_at,
    })
}

/// The owner's side of a bootstrap invite, run by `racking-server invite` on the
/// server machine. Whoever can open the data directory controls the server
/// anyway, so it needs no token; the joiner still confirms like any other.
pub struct Bootstrap {
    store: Store,
    roster: Roster,
}

/// A bootstrap invite whose code was redeemed, as the owner sees it before confirming.
pub struct Redeemed {
    pub name: String,
    pub role: String,
    pub peer: String,
    pub os: String,
    pub rack_version: String,
    pub fingerprint: String,
    pub confirm_by: String,
}

/// What became of a bootstrap invite while the owner waited.
pub enum Waited {
    Open,
    Redeemed(Redeemed),
    Gone(&'static str),
}

const OWNER: &str = "owner";

impl Bootstrap {
    /// Opens the database of `config` alongside a running server; SQLite in WAL mode allows it.
    ///
    /// # Errors
    ///
    /// When the database cannot be opened or read.
    pub fn open(config: &Config) -> Result<Self, String> {
        let store = Store::open(&config.data_dir)?;
        let managed = store.managed_agents()?;
        let mut names: Vec<AgentName> = config.agents.iter().map(|a| a.name.clone()).collect();
        let mut main = config
            .agents
            .iter()
            .find(|a| a.role == Role::Main)
            .map(|a| a.name.clone());
        for row in managed {
            let name: AgentName = row.name.parse()?;
            if row.role == Role::Main.as_str() && main.is_none() {
                main = Some(name.clone());
            }
            names.push(name);
        }
        let roster = Roster {
            names,
            main,
            roles_enforced: config.send_rules.enforced(),
        };
        Ok(Self { store, roster })
    }

    /// Makes an invite with no inviter agent and returns its id, the code (shown
    /// once), the agent name and when it expires.
    ///
    /// # Errors
    ///
    /// When the role or name rules refuse it, or the database fails.
    pub fn invite(&self, name: Option<&str>, role: &str) -> Result<(i64, String, String, String), String> {
        let name = name.map(str::parse::<AgentName>).transpose()?;
        let role: Role = role.parse()?;
        let made = make_invite(
            &self.store,
            &self.roster,
            None,
            name,
            role,
            DEFAULT_TTL_SECS,
            clock::now(),
        )
        .map_err(|e| format!("{}\nhint: {}", e.message, e.hint))?;
        self.audit("invite.create", Some(made.name.as_str()), "ok");
        tracing::info!(target: "racking::pairing", invite = made.id, agent = %made.name, role = %made.role, "bootstrap invite made");
        Ok((
            made.id,
            made.code.to_string(),
            made.name.to_string(),
            clock::rfc3339(made.expires_at),
        ))
    }

    /// Where invite `id` stands now.
    ///
    /// # Errors
    ///
    /// When the database fails or the invite does not exist.
    pub fn poll(&self, id: i64) -> Result<Waited, String> {
        let now = clock::now();
        let invite = self
            .store
            .invite(id, now)
            .map_err(|e| e.message)?
            .ok_or_else(|| format!("no invite {id}"))?;
        Ok(match invite.state {
            InviteState::Open => Waited::Open,
            InviteState::Redeemed => Waited::Redeemed(Redeemed {
                fingerprint: invite.fingerprint().unwrap_or_default(),
                confirm_by: invite.confirm_by().map(clock::rfc3339).unwrap_or_default(),
                name: invite.name.to_string(),
                role: invite.role.to_string(),
                peer: invite.peer.unwrap_or_default(),
                os: invite.os.unwrap_or_else(|| "?".to_owned()),
                rack_version: invite.rack_version.unwrap_or_else(|| "?".to_owned()),
            }),
            InviteState::Expired => Waited::Gone("expired"),
            InviteState::Voided => Waited::Gone("voided"),
            InviteState::Confirmed => Waited::Gone("confirmed"),
            InviteState::Denied => Waited::Gone("denied"),
        })
    }

    /// Confirms or denies a redeemed invite. The running server picks the answer up
    /// on the joiner's next request.
    ///
    /// # Errors
    ///
    /// When the invite is not waiting for a confirmation, or it came too late.
    pub fn decide(&self, id: i64, confirm: bool) -> Result<(), String> {
        let (decision, action) = if confirm {
            (Decision::Confirm, "invite.confirm")
        } else {
            (Decision::Deny, "invite.deny")
        };
        let result = self.store.decide(id, decision, clock::now());
        let target = id.to_string();
        match result {
            Ok(_) => {
                self.audit(action, Some(&target), "ok");
                Ok(())
            }
            Err(e) => {
                self.audit(action, Some(&target), e.code.as_str());
                Err(format!("{}\nhint: {}", e.message, e.hint))
            }
        }
    }

    /// Voids an invite the owner gave up on before it was redeemed.
    ///
    /// # Errors
    ///
    /// When the invite is no longer open.
    pub fn cancel(&self, id: i64) -> Result<(), String> {
        self.store
            .void_invite(id, clock::now())
            .map(drop)
            .map_err(|e| e.message)?;
        self.audit("invite.cancel", Some(&id.to_string()), "ok");
        Ok(())
    }

    fn audit(&self, action: &str, target: Option<&str>, result: &str) {
        let entry = AuditEntry {
            actor: OWNER,
            action,
            target,
            result,
        };
        if let Err(e) = self.store.audit(&entry) {
            tracing::warn!(target: "racking::pairing", action, error = %e.message, "could not write the audit log");
        }
    }
}
