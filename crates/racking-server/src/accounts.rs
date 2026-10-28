//! Creating agents and issuing, rotating and revoking their tokens, apart from
//! any HTTP route so another endpoint (pairing, #46) can reuse it. A token is
//! shown once, to the caller of these functions; only its SHA-256 is stored.

use std::fmt;

use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy::{Links, Role};
use crate::presence::Presence;
use crate::registry::{self, Registry, Standing};
use crate::store::Store;
use crate::types::{AgentName, hex};

/// Random bytes per token; as hex that is 64 chars, well above what the config demands of a token.
const TOKEN_BYTES: usize = 32;

/// A freshly issued token. It has no `Debug` and its `Display` is the secret
/// itself, so it reaches a log only if someone formats it on purpose.
pub struct Token(String);

impl Token {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything the creation or rotation of an agent hands back.
pub struct Issued {
    pub name: AgentName,
    pub role: Role,
    pub token: Token,
}

/// The services an account change touches.
pub struct Accounts<'a> {
    pub registry: &'a Registry,
    pub store: &'a Store,
    pub presence: &'a Presence,
    pub links: &'a Links,
    /// Off for a config from before roles: there, the API may not make a main agent,
    /// since nothing would limit who that agent's workers may send to.
    pub roles_enforced: bool,
}

impl Accounts<'_> {
    /// Creates an agent with a new token, usable at once and without a restart.
    pub fn create(&self, name: &AgentName, role: Role) -> ApiResult<Issued> {
        self.issue(name, role, Standing::Active, |digest, now| {
            self.store.insert_managed(name, role, digest, now)
        })
    }

    /// Creates an agent that may do nothing but ask for its standing until its
    /// inviter confirms it. `persist` stores it, given the token's SHA-256 and the
    /// time, while the registry is locked against a second agent of the same name.
    pub fn create_pending(
        &self,
        name: &AgentName,
        role: Role,
        persist: impl FnOnce(&[u8; 32], i64) -> ApiResult<()>,
    ) -> ApiResult<Issued> {
        self.issue(name, role, Standing::Pending, persist)
    }

    fn issue(
        &self,
        name: &AgentName,
        role: Role,
        standing: Standing,
        persist: impl FnOnce(&[u8; 32], i64) -> ApiResult<()>,
    ) -> ApiResult<Issued> {
        if role == Role::Main && !self.roles_enforced {
            return Err(main_needs_roles());
        }
        let token = new_token()?;
        let digest = registry::digest(token.as_str());
        let now = clock::now();
        self.registry
            .insert(name, role, digest, now, standing, || persist(&digest, now))?;
        self.presence.add(name);
        Ok(Issued {
            name: name.clone(),
            role,
            token,
        })
    }

    /// Gives an API agent a new token; the old one stops working at once.
    pub fn rotate(&self, name: &AgentName) -> ApiResult<Issued> {
        let token = new_token()?;
        let digest = registry::digest(token.as_str());
        let now = clock::now();
        let role = self
            .registry
            .replace_token(name, digest, now, || self.store.set_managed_token(name, &digest, now))?;
        Ok(Issued {
            name: name.clone(),
            role,
            token,
        })
    }

    /// Revokes an API agent's token and forgets the agent and its links; its jobs are kept.
    pub fn revoke(&self, name: &AgentName) -> ApiResult<()> {
        self.registry.remove(name, || self.store.delete_managed(name))?;
        self.forget(name);
        Ok(())
    }

    /// Drops from memory an API agent the store already deleted, such as a denied joiner.
    pub fn forget(&self, name: &AgentName) {
        // An agent already gone from the registry is the outcome wanted, so `NotFound` is fine here.
        let _ = self.registry.remove(name, || Ok(()));
        self.presence.remove(name);
        self.links.forget(name);
    }
}

pub fn main_needs_roles() -> ApiError {
    ApiError::new(
        Code::Conflict,
        "roles are not configured on this server, so no agent may be made main here",
        "Set `role` on the agents of the config file first, or create this one as a worker.",
    )
}

/// `TOKEN_BYTES` from the OS random source, as lowercase hex.
fn new_token() -> ApiResult<Token> {
    let mut bytes = [0u8; TOKEN_BYTES];
    os_random(&mut bytes).map_err(|e| ApiError::internal("read the OS random source", &e))?;
    Ok(Token(hex(&bytes)))
}

/// The server runs on Unix only (see SECURITY.md), where `/dev/urandom` never
/// blocks once the kernel pool is seeded at boot and needs no extra crate.
#[cfg(unix)]
pub fn os_random(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(not(unix))]
pub fn os_random(_buf: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "issuing tokens needs Unix (/dev/urandom); run racking-server on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::config::Config;
    use crate::store::AuditEntry;

    struct Fixture {
        dir: std::path::PathBuf,
        store: Store,
        registry: Registry,
        presence: Presence,
        links: Links,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("racking-accounts-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let config = Config::parse(&format!(
                "data_dir = {:?}\n[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nrole = \"main\"\n\
                 admin = true\n[agents.w1]\ntoken = \"w1-token-0123456789abcdef00\"\n",
                dir.display().to_string()
            ))
            .expect("config");
            let store = Store::open(&dir).expect("store");
            let registry = Registry::new(&config, store.managed_agents().expect("rows")).expect("registry");
            let presence = Presence::new(registry.names().iter(), Vec::new(), Clock::manual(0));
            Self {
                dir,
                store,
                registry,
                presence,
                links: Links::new([]),
            }
        }

        fn accounts(&self) -> Accounts<'_> {
            Accounts {
                registry: &self.registry,
                store: &self.store,
                presence: &self.presence,
                links: &self.links,
                roles_enforced: true,
            }
        }

        /// Every byte of the database, its WAL included, to look for secrets.
        fn database_bytes(&self) -> Vec<u8> {
            ["racking.db", "racking.db-wal"]
                .iter()
                .filter_map(|f| std::fs::read(self.dir.join(f)).ok())
                .flatten()
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn name(n: &str) -> AgentName {
        n.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn rotate_and_revoke_take_effect_at_once() {
        let f = Fixture::new("cycle");
        let a = f.accounts();
        let mac = name("mac");
        let first = a.create(&mac, Role::Worker).expect("create");
        let caller = f.registry.authenticate(first.token.as_str()).expect("new token works");
        assert_eq!(
            (caller.name, caller.role, caller.admin),
            (mac.clone(), Role::Worker, false)
        );
        assert!(f.presence.agent_json(&mac).is_object(), "listed in presence");

        let second = a.rotate(&mac).expect("rotate");
        assert!(
            f.registry.authenticate(first.token.as_str()).is_none(),
            "old token is dead"
        );
        assert!(f.registry.authenticate(second.token.as_str()).is_some());

        a.revoke(&mac).expect("revoke");
        assert!(f.registry.authenticate(second.token.as_str()).is_none());
        assert!(f.presence.agent_json(&mac).is_null());
        assert_eq!(a.rotate(&mac).err().map(|e| e.code), Some(Code::NotFound));
    }

    #[test]
    fn file_agents_and_taken_names_are_refused() {
        let f = Fixture::new("refuse");
        let a = f.accounts();
        assert_eq!(
            a.create(&name("w1"), Role::Worker).err().map(|e| e.code),
            Some(Code::Conflict)
        );
        assert_eq!(
            a.create(&name("x"), Role::Main).err().map(|e| e.code),
            Some(Code::Conflict),
            "boss is main"
        );
        assert_eq!(a.revoke(&name("boss")).err().map(|e| e.code), Some(Code::NotAllowed));
        assert_eq!(a.rotate(&name("w1")).err().map(|e| e.code), Some(Code::NotAllowed));
        let legacy = Accounts {
            roles_enforced: false,
            ..f.accounts()
        };
        assert_eq!(
            legacy.create(&name("y"), Role::Main).err().map(|e| e.code),
            Some(Code::Conflict)
        );
    }

    #[test]
    fn tokens_never_reach_the_database_in_clear() {
        let f = Fixture::new("clear");
        let a = f.accounts();
        let issued = a.create(&name("mac"), Role::Worker).expect("create");
        let rotated = a.rotate(&name("mac")).expect("rotate");
        f.store
            .audit(&AuditEntry {
                actor: "boss",
                action: "agent.rotate",
                target: Some("mac"),
                result: "ok",
            })
            .expect("audit");
        let bytes = f.database_bytes();
        assert!(!bytes.is_empty());
        for token in [issued.token.as_str(), rotated.token.as_str()] {
            assert!(!contains(&bytes, token), "a token is in the database");
        }
        let audit = f.store.audit_page(None, 10).expect("audit").to_string();
        assert!(!audit.contains(rotated.token.as_str()));
        let reopened = Registry::new(
            &Config::parse(&format!(
                "data_dir = {:?}\n[agents.boss]\ntoken = \"boss-token-0123456789abcdef\"\nrole = \"main\"\n\
                 [agents.w1]\ntoken = \"w1-token-0123456789abcdef00\"\n",
                f.dir.display().to_string()
            ))
            .expect("config"),
            f.store.managed_agents().expect("rows"),
        )
        .expect("registry");
        assert!(
            reopened.authenticate(rotated.token.as_str()).is_some(),
            "the hash survives a restart"
        );
    }

    #[test]
    fn tokens_are_long_random_hex() {
        let (a, b) = (new_token().expect("token"), new_token().expect("token"));
        assert_eq!(a.as_str().len(), TOKEN_BYTES * 2);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
    }
}
