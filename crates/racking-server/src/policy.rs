//! Who may send to whom (decision #44). The server gives each agent a role from
//! its config, never from what a request says, so a job's `from_role` is as
//! trustworthy as the token that sent it.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::types::AgentName;

/// An agent's role. `Main` is the one agent the owner talks to; a job it sends
/// carries the owner's delegated authority. Everything a `Worker` sends is data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Main,
    Worker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Worker => "worker",
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "main" => Ok(Role::Main),
            "worker" => Ok(Role::Worker),
            other => Err(format!("role `{other}` must be main or worker")),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The send rules of the server config.
pub struct SendRules {
    /// Off for a config that sets no role and no `[permissions]`: such a file was
    /// written before roles existed, and its agents could all send to each other.
    enforced: bool,
    /// `[permissions] send`: per sender, the only agents it may send to. A sender
    /// left out follows the role defaults.
    explicit: BTreeMap<AgentName, Vec<AgentName>>,
}

impl SendRules {
    pub fn new(enforced: bool, explicit: BTreeMap<AgentName, Vec<AgentName>>) -> Self {
        Self { enforced, explicit }
    }

    pub fn enforced(&self) -> bool {
        self.enforced
    }

    pub fn explicit(&self) -> &BTreeMap<AgentName, Vec<AgentName>> {
        &self.explicit
    }

    /// Whether `from` may start a job for `to`. Defaults: the main agent may send to
    /// anyone, a worker only to the main agent.
    pub fn may_send(&self, from: &AgentName, from_role: Role, to: &AgentName, to_role: Role) -> bool {
        if !self.enforced {
            return true;
        }
        match self.explicit.get(from) {
            Some(allowed) => allowed.contains(to),
            None => from_role == Role::Main || to_role == Role::Main,
        }
    }

    /// Names in `explicit` that are not agents of the server; their rules can never match.
    pub fn unknown_names<'a>(&'a self, known: &'a [AgentName]) -> impl Iterator<Item = &'a AgentName> + 'a {
        self.explicit
            .iter()
            .flat_map(|(from, to)| std::iter::once(from).chain(to))
            .filter(move |n| !known.contains(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> AgentName {
        n.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    #[test]
    fn defaults_let_workers_reach_only_main() {
        let rules = SendRules::new(true, BTreeMap::new());
        let (boss, w1, w2) = (name("boss"), name("w1"), name("w2"));
        assert!(rules.may_send(&boss, Role::Main, &w1, Role::Worker));
        assert!(rules.may_send(&w1, Role::Worker, &boss, Role::Main));
        assert!(!rules.may_send(&w1, Role::Worker, &w2, Role::Worker));
    }

    #[test]
    fn explicit_rules_replace_the_defaults_of_their_sender() {
        let rules = SendRules::new(true, BTreeMap::from([(name("w1"), vec![name("w2")])]));
        let (boss, w1, w2) = (name("boss"), name("w1"), name("w2"));
        assert!(rules.may_send(&w1, Role::Worker, &w2, Role::Worker));
        assert!(
            !rules.may_send(&w1, Role::Worker, &boss, Role::Main),
            "the list is all w1 may reach"
        );
        assert!(
            !rules.may_send(&w2, Role::Worker, &w1, Role::Worker),
            "w2 keeps the defaults"
        );
    }

    #[test]
    fn legacy_configs_allow_everything() {
        let rules = SendRules::new(false, BTreeMap::new());
        assert!(rules.may_send(&name("a"), Role::Worker, &name("b"), Role::Worker));
    }

    #[test]
    fn roles_parse() {
        assert_eq!("main".parse(), Ok(Role::Main));
        assert_eq!("worker".parse(), Ok(Role::Worker));
        assert!("admin".parse::<Role>().is_err());
    }
}
