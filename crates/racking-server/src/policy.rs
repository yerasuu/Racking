//! Who may send to whom: roles (decision #44) and links (#46). The server gives
//! each agent a role from its config, never from what a request says, so a job's
//! `from_role` is as trustworthy as the token that sent it; and an agent made
//! through the API reaches only the agents it was linked with.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::{PoisonError, RwLock};

use crate::registry::Source;
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

/// One side of a send, as the registry knows it.
pub struct Party<'a> {
    pub name: &'a AgentName,
    pub role: Role,
    pub source: Source,
    /// False while a joined agent waits for its inviter to compare fingerprints.
    pub confirmed: bool,
}

/// The confirmed links between agents, mirrored from the store so a send checks
/// them without a query. Written only when a link is confirmed or removed.
pub struct Links {
    pairs: RwLock<BTreeSet<(AgentName, AgentName)>>,
}

/// A link has no direction, so each pair is kept in name order.
fn pair(a: &AgentName, b: &AgentName) -> (AgentName, AgentName) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

impl Links {
    pub fn new(pairs: impl IntoIterator<Item = (AgentName, AgentName)>) -> Self {
        Self {
            pairs: RwLock::new(pairs.into_iter().map(|(a, b)| pair(&a, &b)).collect()),
        }
    }

    pub fn contains(&self, a: &AgentName, b: &AgentName) -> bool {
        self.pairs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&pair(a, b))
    }

    pub fn insert(&self, a: &AgentName, b: &AgentName) {
        self.pairs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pair(a, b));
    }

    pub fn remove(&self, a: &AgentName, b: &AgentName) {
        self.pairs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pair(a, b));
    }

    /// Drops every link of an agent that no longer exists, so a later agent of the same name inherits none.
    pub fn forget(&self, name: &AgentName) {
        self.pairs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(a, b)| a != name && b != name);
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

    /// Whether `from` may start a job for `to`: both confirmed, linked, and allowed
    /// by the roles. An agent of the config file counts as linked with every other
    /// agent, as before links existed, so links only constrain agents made through
    /// the API.
    pub fn may_send(&self, from: &Party<'_>, to: &Party<'_>, links: &Links) -> bool {
        let linked = from.source == Source::File || to.source == Source::File || links.contains(from.name, to.name);
        from.confirmed && to.confirmed && linked && self.roles_allow(from.name, from.role, to.name, to.role)
    }

    /// The role half of `may_send`. Defaults: the main agent may send to anyone,
    /// a worker only to the main agent.
    pub(crate) fn roles_allow(&self, from: &AgentName, from_role: Role, to: &AgentName, to_role: Role) -> bool {
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

    fn party(name: &AgentName, role: Role, source: Source) -> Party<'_> {
        Party {
            name,
            role,
            source,
            confirmed: true,
        }
    }

    #[test]
    fn defaults_let_workers_reach_only_main() {
        let rules = SendRules::new(true, BTreeMap::new());
        let (boss, w1, w2) = (name("boss"), name("w1"), name("w2"));
        assert!(rules.roles_allow(&boss, Role::Main, &w1, Role::Worker));
        assert!(rules.roles_allow(&w1, Role::Worker, &boss, Role::Main));
        assert!(!rules.roles_allow(&w1, Role::Worker, &w2, Role::Worker));
    }

    #[test]
    fn explicit_rules_replace_the_defaults_of_their_sender() {
        let rules = SendRules::new(true, BTreeMap::from([(name("w1"), vec![name("w2")])]));
        let (boss, w1, w2) = (name("boss"), name("w1"), name("w2"));
        assert!(rules.roles_allow(&w1, Role::Worker, &w2, Role::Worker));
        assert!(
            !rules.roles_allow(&w1, Role::Worker, &boss, Role::Main),
            "the list is all w1 may reach"
        );
        assert!(
            !rules.roles_allow(&w2, Role::Worker, &w1, Role::Worker),
            "w2 keeps the defaults"
        );
    }

    #[test]
    fn legacy_configs_allow_everything() {
        let rules = SendRules::new(false, BTreeMap::new());
        let (a, b) = (name("a"), name("b"));
        assert!(rules.may_send(
            &party(&a, Role::Worker, Source::File),
            &party(&b, Role::Worker, Source::File),
            &Links::new([])
        ));
    }

    #[test]
    fn api_agents_need_a_confirmed_link() {
        let rules = SendRules::new(true, BTreeMap::new());
        let (boss, mac, pc) = (name("boss"), name("mac"), name("pc"));
        let links = Links::new([]);
        let api_main = party(&boss, Role::Main, Source::Api);
        let mac_w = party(&mac, Role::Worker, Source::Api);
        assert!(!rules.may_send(&api_main, &mac_w, &links), "two API agents, no link");
        assert!(!rules.may_send(&mac_w, &api_main, &links));
        links.insert(&mac, &boss);
        assert!(rules.may_send(&api_main, &mac_w, &links), "a link has no direction");
        assert!(rules.may_send(&mac_w, &api_main, &links));

        let pc_w = party(&pc, Role::Worker, Source::Api);
        links.insert(&mac, &pc);
        assert!(
            !rules.may_send(&mac_w, &pc_w, &links),
            "a link does not lift the role rules"
        );
        links.forget(&mac);
        assert!(!rules.may_send(&api_main, &mac_w, &links), "forgotten with its agent");
    }

    #[test]
    fn config_agents_are_linked_with_everyone_but_never_with_the_unconfirmed() {
        let rules = SendRules::new(true, BTreeMap::new());
        let (boss, mac) = (name("boss"), name("mac"));
        let links = Links::new([]);
        let file_main = party(&boss, Role::Main, Source::File);
        let mut mac_w = party(&mac, Role::Worker, Source::Api);
        assert!(rules.may_send(&file_main, &mac_w, &links));
        assert!(rules.may_send(&mac_w, &file_main, &links));
        mac_w.confirmed = false;
        assert!(
            !rules.may_send(&file_main, &mac_w, &links),
            "nothing reaches a pending agent"
        );
        assert!(
            !rules.may_send(&mac_w, &file_main, &links),
            "a pending agent sends nothing"
        );
    }

    #[test]
    fn roles_parse() {
        assert_eq!("main".parse(), Ok(Role::Main));
        assert_eq!("worker".parse(), Ok(Role::Worker));
        assert!("admin".parse::<Role>().is_err());
    }
}
