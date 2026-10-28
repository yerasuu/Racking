//! The agents a token can belong to, with their roles: those of the config file
//! and those the admin API made. Every request reads it to authenticate; only the
//! admin API writes it, so it sits behind a `RwLock`.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy::{Party, Role};
use crate::store::Managed;
use crate::types::AgentName;

/// The authenticated agent of a request, as the server knows it.
#[derive(Clone)]
pub struct Caller {
    pub name: AgentName,
    pub role: Role,
    pub admin: bool,
    pub source: Source,
    /// Joined with a code whose inviter has not confirmed the fingerprint yet.
    pub pending: bool,
}

impl Caller {
    pub fn party(&self) -> Party<'_> {
        Party {
            name: &self.name,
            role: self.role,
            source: self.source,
            confirmed: !self.pending,
        }
    }
}

/// An agent as `may_send` needs it, owned so it outlives the registry lock.
pub struct Member {
    pub name: AgentName,
    pub role: Role,
    pub source: Source,
    pub pending: bool,
}

impl Member {
    pub fn party(&self) -> Party<'_> {
        Party {
            name: &self.name,
            role: self.role,
            source: self.source,
            confirmed: !self.pending,
        }
    }
}

/// Where an agent is defined, which decides who may change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The config file: only the owner editing it changes the agent.
    File,
    /// The admin API: rotated and removed through it, never an admin.
    Api,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::File => "file",
            Source::Api => "api",
        }
    }
}

struct Entry {
    name: AgentName,
    role: Role,
    admin: bool,
    source: Source,
    /// When the API issued the current token; unknown for file agents.
    token_created_at: Option<i64>,
    /// SHA-256 of the token. Tokens are compared by digest so the comparison
    /// takes the same time whatever the length or content of the presented token.
    digest: [u8; 32],
    pending: bool,
}

impl Entry {
    fn member(&self) -> Member {
        Member {
            name: self.name.clone(),
            role: self.role,
            source: self.source,
            pending: self.pending,
        }
    }
}

/// One agent for `GET /admin/agents`: everything but the token digest.
pub struct Listed {
    pub name: AgentName,
    pub role: Role,
    pub admin: bool,
    pub source: Source,
    pub token_created_at: Option<i64>,
    pub pending: bool,
}

/// Whether a new API agent may work at once, or waits for its inviter's confirmation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Active,
    Pending,
}

pub struct Registry {
    entries: RwLock<Vec<Entry>>,
}

pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Registry {
    /// File agents first, then the stored API agents. An API agent whose name the
    /// file took since is skipped with a warning, since the file is the owner's word.
    ///
    /// # Errors
    ///
    /// When a stored row is corrupt, or the API made a main agent and the file now names another.
    pub fn new(config: &Config, managed: Vec<Managed>) -> Result<Self, String> {
        let mut entries: Vec<Entry> = config
            .agents
            .iter()
            .map(|a| Entry {
                name: a.name.clone(),
                role: a.role,
                admin: a.admin,
                source: Source::File,
                token_created_at: None,
                digest: digest(&a.token),
                pending: false,
            })
            .collect();
        for row in managed {
            let name: AgentName = row.name.parse().map_err(|e| format!("managed_agents: {e}"))?;
            let role: Role = row.role.parse().map_err(|e| format!("managed_agents.{name}: {e}"))?;
            if entries.iter().any(|e| e.name == name) {
                tracing::warn!(
                    target: "racking::admin",
                    agent = %name,
                    "an agent of the config file has the name of one made through the admin API; the file wins"
                );
                continue;
            }
            if role == Role::Main
                && let Some(main) = entries.iter().find(|e| e.role == Role::Main)
            {
                return Err(format!(
                    "{name} was made main through the admin API and {} is main too; only one agent may be main",
                    main.name
                ));
            }
            entries.push(Entry {
                name,
                role,
                admin: false,
                source: Source::Api,
                token_created_at: Some(row.token_created_at),
                digest: row.digest,
                pending: row.pending,
            });
        }
        Ok(Self {
            entries: RwLock::new(entries),
        })
    }

    /// A panic while holding the lock cannot leave the list half changed: every
    /// write is one push, remove or field store, after the store write succeeded.
    fn read(&self) -> RwLockReadGuard<'_, Vec<Entry>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Vec<Entry>> {
        self.entries.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Checks every agent without stopping early, so timing does not reveal which one matched.
    pub fn authenticate(&self, token: &str) -> Option<Caller> {
        let presented = digest(token);
        let mut found = None;
        for entry in self.read().iter() {
            let diff = entry
                .digest
                .iter()
                .zip(presented.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b));
            if diff == 0 {
                found = Some(Caller {
                    name: entry.name.clone(),
                    role: entry.role,
                    admin: entry.admin,
                    source: entry.source,
                    pending: entry.pending,
                });
            }
        }
        found
    }

    pub fn role_of(&self, name: &AgentName) -> Option<Role> {
        self.read().iter().find(|e| &e.name == name).map(|e| e.role)
    }

    pub fn member(&self, name: &AgentName) -> Option<Member> {
        self.read().iter().find(|e| &e.name == name).map(Entry::member)
    }

    pub fn names(&self) -> Vec<AgentName> {
        self.read().iter().map(|e| e.name.clone()).collect()
    }

    /// Every agent, in registry order.
    pub fn members(&self) -> Vec<Member> {
        self.read().iter().map(Entry::member).collect()
    }

    /// The main agent, a pending one included, since confirming it would make a second.
    pub fn main_agent(&self) -> Option<AgentName> {
        self.read()
            .iter()
            .find(|e| e.role == Role::Main)
            .map(|e| e.name.clone())
    }

    pub fn list(&self) -> Vec<Listed> {
        self.read()
            .iter()
            .map(|e| Listed {
                name: e.name.clone(),
                role: e.role,
                admin: e.admin,
                source: e.source,
                token_created_at: e.token_created_at,
                pending: e.pending,
            })
            .collect()
    }

    /// Lets a pending agent work, once its inviter confirmed the fingerprint.
    pub fn confirm(&self, name: &AgentName) {
        if let Some(e) = self.write().iter_mut().find(|e| &e.name == name) {
            e.pending = false;
        }
    }

    /// Adds an API agent once `persist` stored it. The lock is held across `persist`
    /// so two adds cannot both pass the name and main checks.
    pub fn insert(
        &self,
        name: &AgentName,
        role: Role,
        digest: [u8; 32],
        now: i64,
        standing: Standing,
        persist: impl FnOnce() -> ApiResult<()>,
    ) -> ApiResult<()> {
        let mut entries = self.write();
        if entries.iter().any(|e| &e.name == name) {
            return Err(ApiError::new(
                Code::Conflict,
                format!("an agent named {name} exists already"),
                "Pick another name; GET /admin/agents lists the taken ones.",
            ));
        }
        if role == Role::Main
            && let Some(main) = entries.iter().find(|e| e.role == Role::Main)
        {
            return Err(ApiError::new(
                Code::Conflict,
                format!("{} is main already", main.name),
                "Only one agent may be main; create this one as a worker.",
            ));
        }
        persist()?;
        entries.push(Entry {
            name: name.clone(),
            role,
            admin: false,
            source: Source::Api,
            token_created_at: Some(now),
            digest,
            pending: standing == Standing::Pending,
        });
        Ok(())
    }

    /// Replaces an API agent's token once `persist` stored it; the old one fails from the next request on.
    pub fn replace_token(
        &self,
        name: &AgentName,
        digest: [u8; 32],
        now: i64,
        persist: impl FnOnce() -> ApiResult<()>,
    ) -> ApiResult<Role> {
        let mut entries = self.write();
        let entry = api_entry(&mut entries, name)?;
        persist()?;
        entry.digest = digest;
        entry.token_created_at = Some(now);
        Ok(entry.role)
    }

    /// Removes an API agent once `persist` deleted it; its token fails from the next request on.
    pub fn remove(&self, name: &AgentName, persist: impl FnOnce() -> ApiResult<()>) -> ApiResult<()> {
        let mut entries = self.write();
        api_entry(&mut entries, name)?;
        persist()?;
        entries.retain(|e| &e.name != name);
        Ok(())
    }
}

/// The API agent `name`; a file agent is refused, since its token lives in the owner's file.
fn api_entry<'a>(entries: &'a mut [Entry], name: &AgentName) -> ApiResult<&'a mut Entry> {
    let Some(entry) = entries.iter_mut().find(|e| &e.name == name) else {
        return Err(ApiError::new(
            Code::NotFound,
            format!("no agent named {name}"),
            "GET /admin/agents lists the agents.",
        ));
    };
    if entry.source == Source::File {
        return Err(ApiError::new(
            Code::NotAllowed,
            format!("{name} is defined in the server config file"),
            "Change or remove it by editing the config file and restarting the server.",
        ));
    }
    Ok(entry)
}
