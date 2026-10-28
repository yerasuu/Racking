//! Server configuration, read once at startup from a TOML file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::net::{self, Cidr};
use crate::policy::{Role, SendRules};
use crate::types::AgentName;

/// The longest `shutdown_grace_secs`: past a minute a supervisor would kill the
/// server anyway (Docker waits 10 s unless `stop_grace_period` says otherwise).
const MAX_GRACE_SECS: u64 = 60;

/// Tokens shorter than this are refused, so a placeholder or a typo cannot become a credential.
const MIN_TOKEN_LEN: usize = 24;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default)]
    allow_public_bind: bool,
    #[serde(default)]
    allow_from: Option<Vec<String>>,
    #[serde(default)]
    trusted_proxies: Vec<String>,
    #[serde(default = "default_grace")]
    shutdown_grace_secs: u64,
    data_dir: PathBuf,
    agents: BTreeMap<String, RawAgent>,
    #[serde(default)]
    permissions: Option<RawPermissions>,
    #[serde(default)]
    limits: SenderLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissions {
    #[serde(default)]
    send: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    token: String,
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    role: Option<String>,
}

fn default_grace() -> u64 {
    10
}

fn default_bind() -> String {
    "127.0.0.1:7878".to_owned()
}

/// A checked configuration: at least two agents, valid names, distinct real tokens.
pub struct Config {
    pub bind: SocketAddr,
    /// Set when `bind` is neither loopback nor private and the owner said so with
    /// `allow_public_bind = true`; the server then warns at every start.
    pub public_bind: bool,
    /// Peers that may connect at all; any other gets 403 before authentication.
    pub allow_from: Vec<Cidr>,
    /// Reverse proxies whose `X-Forwarded-For` names the client; empty trusts no
    /// forwarded header, which is right whenever clients reach the port directly.
    pub trusted_proxies: Vec<Cidr>,
    /// How long a SIGTERM or SIGINT lets requests in flight finish before the
    /// database is checkpointed and the server exits anyway.
    pub shutdown_grace: Duration,
    pub data_dir: PathBuf,
    pub agents: Vec<Agent>,
    pub send_rules: SendRules,
    pub limits: SenderLimits,
}

/// `[limits]`: per-sender guards against an agent that sends without end.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SenderLimits {
    /// Tasks not yet done, failed or rejected from one sender to one recipient.
    pub open_tasks_per_pair: u32,
    /// Jobs of any kind one sender may create per wall-clock minute.
    pub jobs_per_minute: u32,
}

impl Default for SenderLimits {
    fn default() -> Self {
        Self {
            open_tasks_per_pair: 20,
            jobs_per_minute: 60,
        }
    }
}

pub struct Agent {
    pub name: AgentName,
    pub token: String,
    /// May publish `rack` binaries (`PUT /dist/...`); off unless the config says so.
    pub admin: bool,
    /// `worker` unless the config says `main`; at most one agent is `main`.
    pub role: Role,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let bind: SocketAddr = raw.bind.parse().map_err(|e| format!("bind `{}`: {e}", raw.bind))?;
        let public_bind = !net::is_local(bind.ip());
        if public_bind && !raw.allow_public_bind {
            return Err(format!(
                "bind `{bind}` is not a loopback or private LAN address; Racking is LAN only. Bind the machine's \
                 LAN address, or set allow_public_bind = true if the owner really wants this"
            ));
        }
        let allow_from = match &raw.allow_from {
            Some(list) => list
                .iter()
                .map(String::as_str)
                .map(str::parse)
                .collect::<Result<Vec<Cidr>, String>>(),
            None => net::DEFAULT_ALLOW_FROM.iter().copied().map(str::parse).collect(),
        }
        .map_err(|e: String| format!("allow_from: {e}"))?;
        let trusted_proxies = raw
            .trusted_proxies
            .iter()
            .map(|text| {
                let block: Cidr = text.parse()?;
                if block.is_everything() {
                    return Err(format!(
                        "`{block}` would trust every peer to name its own address; list the proxy's address"
                    ));
                }
                Ok(block)
            })
            .collect::<Result<Vec<Cidr>, String>>()
            .map_err(|e| format!("trusted_proxies: {e}"))?;
        if raw.shutdown_grace_secs > MAX_GRACE_SECS {
            return Err(format!("shutdown_grace_secs must be 0 to {MAX_GRACE_SECS}"));
        }
        if raw.agents.len() < 2 {
            return Err("at least two agents are needed under [agents.NAME]".to_owned());
        }
        let mut agents = Vec::with_capacity(raw.agents.len());
        let mut any_role = false;
        for (name, agent) in raw.agents {
            let name: AgentName = name.parse()?;
            let token = agent.token;
            if token.len() < MIN_TOKEN_LEN || token.contains("CHANGE-ME") {
                return Err(format!(
                    "agents.{name}.token must be a real secret of at least {MIN_TOKEN_LEN} chars"
                ));
            }
            if token.bytes().any(|b| !b.is_ascii_graphic()) {
                return Err(format!("agents.{name}.token must be printable ASCII without spaces"));
            }
            if agents.iter().any(|a: &Agent| a.token == token) {
                return Err(format!("agents.{name}.token is shared with another agent"));
            }
            let role = match agent.role {
                Some(text) => {
                    any_role = true;
                    text.parse().map_err(|e| format!("agents.{name}.role: {e}"))?
                }
                None => Role::Worker,
            };
            if role == Role::Main
                && let Some(other) = agents.iter().find(|a: &&Agent| a.role == Role::Main)
            {
                return Err(format!(
                    "agents.{name}.role: {} is main already; only one agent may be main",
                    other.name
                ));
            }
            agents.push(Agent {
                name,
                token,
                admin: agent.admin,
                role,
            });
        }
        if raw.limits.open_tasks_per_pair == 0 || raw.limits.jobs_per_minute == 0 {
            return Err("limits: open_tasks_per_pair and jobs_per_minute must be at least 1".to_owned());
        }
        let explicit = match &raw.permissions {
            Some(p) => send_lists(&p.send)?,
            None => BTreeMap::new(),
        };
        Ok(Self {
            bind,
            public_bind,
            allow_from,
            trusted_proxies,
            shutdown_grace: Duration::from_secs(raw.shutdown_grace_secs),
            data_dir: raw.data_dir,
            agents,
            send_rules: SendRules::new(any_role || raw.permissions.is_some(), explicit),
            limits: raw.limits,
        })
    }
}

/// `[permissions] send`, with every name checked. Names that are no agent of the
/// file are kept: they may be agents added through the admin API.
fn send_lists(raw: &BTreeMap<String, Vec<String>>) -> Result<BTreeMap<AgentName, Vec<AgentName>>, String> {
    raw.iter()
        .map(|(from, to)| {
            let from: AgentName = from.parse().map_err(|e| format!("permissions.send: {e}"))?;
            let to = to
                .iter()
                .map(|n| n.parse().map_err(|e| format!("permissions.send.{from}: {e}")))
                .collect::<Result<Vec<AgentName>, String>>()?;
            Ok((from, to))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const T2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn cfg(agents: &str) -> Result<Config, String> {
        Config::parse(&format!("data_dir = \"/tmp/x\"\n{agents}"))
    }

    #[test]
    fn defaults_to_loopback() {
        let c = cfg(&format!(
            "[agents.linux]\ntoken = \"{T1}\"\n[agents.windows]\ntoken = \"{T2}\"\n"
        ));
        assert_eq!(c.map(|c| c.bind.to_string()), Ok("127.0.0.1:7878".to_owned()));
    }

    #[test]
    fn public_binds_need_the_owner_s_word() {
        let agents = format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n");
        let at = |bind: &str, extra: &str| {
            Config::parse(&format!("bind = \"{bind}\"\n{extra}data_dir = \"/tmp/x\"\n{agents}"))
        };
        assert!(at("192.168.1.20:7878", "").is_ok_and(|c| !c.public_bind));
        assert!(at("[fe80::1]:7878", "").is_ok());
        let err = at("0.0.0.0:7878", "").err().unwrap_or_default();
        assert!(err.contains("allow_public_bind"), "{err}");
        assert!(at("203.0.113.5:7878", "").is_err());
        assert!(at("0.0.0.0:7878", "allow_public_bind = true\n").is_ok_and(|c| c.public_bind));
    }

    #[test]
    fn allow_from_defaults_to_lan_and_loopback() {
        let agents = format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n");
        let c = cfg(&agents).expect("config");
        let allowed = |ip: &str| {
            let ip = ip.parse().unwrap_or_else(|e| panic!("{e}"));
            c.allow_from.iter().any(|b| b.contains(ip))
        };
        assert!(allowed("127.0.0.1") && allowed("192.168.4.4") && allowed("10.9.9.9") && allowed("::1"));
        assert!(allowed("fe80::abcd"));
        assert!(!allowed("8.8.8.8") && !allowed("2001:db8::1"));
        let only = Config::parse(&format!(
            "allow_from = [\"10.0.0.0/8\"]\ndata_dir = \"/tmp/x\"\n{agents}"
        ))
        .expect("config");
        assert_eq!(only.allow_from.len(), 1);
        assert!(
            Config::parse(&format!(
                "allow_from = [\"10.0.0.0/40\"]\ndata_dir = \"/tmp/x\"\n{agents}"
            ))
            .is_err()
        );
    }

    #[test]
    fn trusted_proxies_default_to_none() {
        let agents = format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n");
        assert!(cfg(&agents).expect("config").trusted_proxies.is_empty());
        let with = |list: &str| Config::parse(&format!("trusted_proxies = {list}\ndata_dir = \"/tmp/x\"\n{agents}"));
        let c = with("[\"172.18.0.0/16\", \"fd00::1\"]").expect("config");
        assert_eq!(c.trusted_proxies.len(), 2);
        assert!(with("[\"172.18.0.0/40\"]").is_err());
        let err = with("[\"0.0.0.0/0\"]").err().unwrap_or_default();
        assert!(err.contains("trusted_proxies"), "{err}");
        assert!(with("[\"::/0\"]").is_err());
    }

    #[test]
    fn shutdown_grace_defaults_to_ten_seconds() {
        let agents = format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n");
        assert_eq!(cfg(&agents).expect("config").shutdown_grace, Duration::from_secs(10));
        let with = |secs: &str| {
            Config::parse(&format!(
                "shutdown_grace_secs = {secs}\ndata_dir = \"/tmp/x\"\n{agents}"
            ))
        };
        assert_eq!(with("0").expect("config").shutdown_grace, Duration::ZERO);
        assert_eq!(with("60").expect("config").shutdown_grace, Duration::from_secs(60));
        assert!(with("61").is_err());
        assert!(with("-1").is_err());
    }

    #[test]
    fn the_example_config_parses_once_tokens_are_filled_in() {
        let example = include_str!("../../../config.example.toml")
            .replace("CHANGE-ME-linux-token", T1)
            .replace("CHANGE-ME-windows-token", T2);
        let c = Config::parse(&example).expect("config.example.toml");
        assert!(c.send_rules.enforced());
        assert!(!c.public_bind);
        assert!(
            Config::parse(include_str!("../../../config.example.toml")).is_err(),
            "placeholders refused"
        );
    }

    #[test]
    fn admin_is_opt_in() {
        let c = cfg(&format!(
            "[agents.linux]\ntoken = \"{T1}\"\nadmin = true\n[agents.windows]\ntoken = \"{T2}\"\n"
        ));
        let admins = c.map(|c| {
            c.agents
                .iter()
                .map(|a| (a.name.to_string(), a.admin))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            admins,
            Ok(vec![("linux".to_owned(), true), ("windows".to_owned(), false)])
        );
    }

    const T3: &str = "cccccccccccccccccccccccccccc";

    fn names(n: &[&str]) -> Vec<AgentName> {
        n.iter()
            .map(|n| n.parse().unwrap_or_else(|e: String| panic!("{e}")))
            .collect()
    }

    #[test]
    fn at_most_one_main() {
        let two = cfg(&format!(
            "[agents.a]\ntoken = \"{T1}\"\nrole = \"main\"\n[agents.b]\ntoken = \"{T2}\"\nrole = \"main\"\n"
        ));
        assert!(two.err().is_some_and(|e| e.contains("only one agent may be main")));
        let bad = cfg(&format!(
            "[agents.a]\ntoken = \"{T1}\"\nrole = \"boss\"\n[agents.b]\ntoken = \"{T2}\"\n"
        ));
        assert!(bad.is_err());
    }

    #[test]
    fn roles_turn_on_the_default_permissions() {
        let c = cfg(&format!(
            "[agents.boss]\ntoken = \"{T1}\"\nrole = \"main\"\n[agents.w1]\ntoken = \"{T2}\"\n\
             [agents.w2]\ntoken = \"{T3}\"\n"
        ))
        .expect("config");
        let [boss, w1, w2] = <[AgentName; 3]>::try_from(names(&["boss", "w1", "w2"])).expect("names");
        let role = |n: &AgentName| c.agents.iter().find(|a| &a.name == n).map(|a| a.role);
        assert_eq!(role(&w1), Some(Role::Worker), "worker is the default");
        assert!(c.send_rules.enforced());
        assert!(c.send_rules.roles_allow(&boss, Role::Main, &w1, Role::Worker));
        assert!(c.send_rules.roles_allow(&w1, Role::Worker, &boss, Role::Main));
        assert!(!c.send_rules.roles_allow(&w1, Role::Worker, &w2, Role::Worker));
    }

    #[test]
    fn no_roles_keeps_everyone_reaching_everyone() {
        let c = cfg(&format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n")).expect("config");
        assert!(!c.send_rules.enforced());
        let [a, b] = <[AgentName; 2]>::try_from(names(&["a", "b"])).expect("names");
        assert!(c.send_rules.roles_allow(&a, Role::Worker, &b, Role::Worker));
    }

    #[test]
    fn explicit_permissions() {
        let c = cfg(&format!(
            "[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n\
             [permissions]\nsend = {{ a = [\"b\"] }}\n"
        ))
        .expect("config");
        assert!(c.send_rules.enforced(), "a [permissions] section turns the rules on");
        let [a, b] = <[AgentName; 2]>::try_from(names(&["a", "b"])).expect("names");
        assert!(c.send_rules.roles_allow(&a, Role::Worker, &b, Role::Worker));
        assert!(!c.send_rules.roles_allow(&b, Role::Worker, &a, Role::Worker));
        let bad = cfg(&format!(
            "[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n\
             [permissions]\nsend = {{ a = [\"B!\"] }}\n"
        ));
        assert!(bad.is_err());
    }

    #[test]
    fn limits_default_and_override() {
        let agents = format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n");
        let c = cfg(&agents).expect("config");
        assert_eq!((c.limits.open_tasks_per_pair, c.limits.jobs_per_minute), (20, 60));
        let c = cfg(&format!("{agents}[limits]\njobs_per_minute = 5\n")).expect("config");
        assert_eq!((c.limits.open_tasks_per_pair, c.limits.jobs_per_minute), (20, 5));
        assert!(cfg(&format!("{agents}[limits]\njobs_per_minute = 0\n")).is_err());
        assert!(cfg(&format!("{agents}[limits]\nbody_bytes = 9\n")).is_err());
    }

    #[test]
    fn refuses_bad_tokens() {
        let placeholder = "CHANGE-ME-xxxxxxxxxxxxxxxxxxxxxxx";
        assert!(
            cfg(&format!(
                "[agents.a]\ntoken = \"{placeholder}\"\n[agents.b]\ntoken = \"{T2}\"\n"
            ))
            .is_err()
        );
        assert!(cfg(&format!("[agents.a]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T1}\"\n")).is_err());
        assert!(
            cfg(&format!(
                "[agents.a]\ntoken = \"short\"\n[agents.b]\ntoken = \"{T2}\"\n"
            ))
            .is_err()
        );
        assert!(cfg(&format!("[agents.A]\ntoken = \"{T1}\"\n[agents.b]\ntoken = \"{T2}\"\n")).is_err());
    }
}
