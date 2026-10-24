//! Server configuration, read once at startup from a TOML file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::AgentName;

/// Tokens shorter than this are refused, so a placeholder or a typo cannot become a credential.
const MIN_TOKEN_LEN: usize = 24;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_bind")]
    bind: String,
    data_dir: PathBuf,
    agents: BTreeMap<String, RawAgent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    token: String,
}

fn default_bind() -> String {
    "127.0.0.1:7878".to_owned()
}

/// A checked configuration: at least two agents, valid names, distinct real tokens.
pub struct Config {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub agents: Vec<Agent>,
}

pub struct Agent {
    pub name: AgentName,
    pub token: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let bind = raw.bind.parse().map_err(|e| format!("bind `{}`: {e}", raw.bind))?;
        if raw.agents.len() < 2 {
            return Err("at least two agents are needed under [agents.NAME]".to_owned());
        }
        let mut agents = Vec::with_capacity(raw.agents.len());
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
            agents.push(Agent { name, token });
        }
        Ok(Self {
            bind,
            data_dir: raw.data_dir,
            agents,
        })
    }
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
