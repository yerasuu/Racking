//! The agents a token can belong to, with their roles. Every request reads it to
//! authenticate; only the admin API writes it, so it sits behind a `RwLock`.

use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::policy::Role;
use crate::types::AgentName;

/// The authenticated agent of a request, as the server knows it.
#[derive(Clone)]
pub struct Caller {
    pub name: AgentName,
    pub role: Role,
    pub admin: bool,
}

pub struct Entry {
    pub name: AgentName,
    pub role: Role,
    pub admin: bool,
    /// SHA-256 of the token. Tokens are compared by digest so the comparison
    /// takes the same time whatever the length or content of the presented token.
    digest: [u8; 32],
}

pub struct Registry {
    entries: RwLock<Vec<Entry>>,
}

pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Registry {
    pub fn from_config(config: &Config) -> Self {
        let entries = config
            .agents
            .iter()
            .map(|a| Entry {
                name: a.name.clone(),
                role: a.role,
                admin: a.admin,
                digest: digest(&a.token),
            })
            .collect();
        Self {
            entries: RwLock::new(entries),
        }
    }

    /// A panic while holding the lock cannot leave the list half changed: every
    /// write is one push, remove or field store.
    fn read(&self) -> RwLockReadGuard<'_, Vec<Entry>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
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
                });
            }
        }
        found
    }

    pub fn role_of(&self, name: &AgentName) -> Option<Role> {
        self.read().iter().find(|e| &e.name == name).map(|e| e.role)
    }

    pub fn names(&self) -> Vec<AgentName> {
        self.read().iter().map(|e| e.name.clone()).collect()
    }

    /// Every agent with its role, in registry order.
    pub fn roles(&self) -> Vec<(AgentName, Role)> {
        self.read().iter().map(|e| (e.name.clone(), e.role)).collect()
    }
}
