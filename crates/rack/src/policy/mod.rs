//! A worker's local policy (decision #44, task #49): what this machine's agent may run on its own
//! for a task that carries the owner's delegated authority, and nothing else. The owner of the
//! machine writes it; no job, message or work directory can change it. See `docs/commands/policy.md`.
//!
//! Every check here fails closed: a policy that cannot be read or does not validate allows nothing.

pub mod decide;
pub mod glob;
pub mod hook;
pub mod journal;
pub mod paths;
pub mod shell;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::client::Result;
use paths::Resolved;

/// `docs/policy-example.toml`: the file `rack policy init` writes, with its comments.
pub const TEMPLATE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/policy-example.toml"));

/// The workdir line of `TEMPLATE` that `rack policy init` replaces with the real one.
pub const TEMPLATE_WORKDIR: &str = "workdir = '/home/you/rack-work'";

/// The only schema version this build reads.
pub const VERSION: u32 = 1;

/// Environment variables a profile may never let a command set: they change which code runs
/// (`LD_PRELOAD`, `PATH`...) or where `rack` takes its config and policy from.
const FORBIDDEN_ENV: &[&str] = &[
    "PATH",
    "IFS",
    "ENV",
    "BASH_ENV",
    "SHELLOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "HOME",
    "XDG_CONFIG_HOME",
    "APPDATA",
];
const FORBIDDEN_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "RACK_", "BASH_FUNC_", "CLAUDE"];

/// A profile name, checked: `[a-z0-9-]{1,32}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProfileName {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let ok = (1..=32).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!("`{s}` is not a profile name ([a-z0-9-], 1 to 32)"))
        }
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the hook answers for a tool call while no profile is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoProfile {
    Deny,
    Ask,
}

/// The policy, validated: every path absolute and resolved, every placeholder expanded.
#[derive(Debug)]
pub struct Policy {
    pub path: PathBuf,
    pub workdir: Resolved,
    pub max_open_jobs: u32,
    pub max_runtime_secs: u64,
    pub max_output_bytes: u64,
    pub max_hops: i64,
    /// Whether tasks from the main agent may start a profile; `accept_from = []` turns it off.
    pub accept_main: bool,
    pub no_profile: NoProfile,
    pub profiles: BTreeMap<ProfileName, Profile>,
}

#[derive(Debug)]
pub struct Profile {
    pub description: String,
    pub enabled: bool,
    pub run: Vec<RunRule>,
    pub read: Vec<Resolved>,
    pub write: Vec<Resolved>,
    /// `host:port` pairs; empty means no network besides the Racking server.
    pub network: Vec<String>,
    pub env_allow: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub max_output_bytes: u64,
    pub returns: Vec<String>,
}

/// One command a profile may run: the exact absolute program and one anchored pattern per argument.
#[derive(Debug)]
pub struct RunRule {
    pub cmd: String,
    pub args: Vec<String>,
}

impl RunRule {
    /// Whether `argv` (program first) is this rule: same program, same number of arguments, each
    /// matching its pattern. A wildcard never matches a `..` component the pattern does not spell
    /// out, so `{workdir}/*` cannot climb out of the workdir.
    #[must_use]
    pub fn matches(&self, argv: &[String]) -> bool {
        let Some((cmd, args)) = argv.split_first() else {
            return false;
        };
        *cmd == self.cmd
            && args.len() == self.args.len()
            && self
                .args
                .iter()
                .zip(args)
                .all(|(pattern, arg)| glob::matches(pattern, arg) && (!has_dotdot(arg) || has_dotdot(pattern)))
    }

    /// `cmd args...` as the worker types it.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(self.cmd.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn has_dotdot(s: &str) -> bool {
    s.split(['/', '\\', '=']).any(|part| part == "..")
}

impl Profile {
    /// Whether a file tool may read `path`: anything it may write it may also read.
    #[must_use]
    pub fn may_read(&self, path: &Resolved) -> bool {
        self.read.iter().chain(&self.write).any(|p| path.covered_by(p))
    }

    #[must_use]
    pub fn may_write(&self, path: &Resolved) -> bool {
        self.write.iter().any(|p| path.covered_by(p))
    }
}

/// Every problem found in a policy file, so `rack policy check` can list them all at once.
#[derive(Debug)]
pub struct Problems(pub Vec<String>);

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("; "))
    }
}

/// `RACK_POLICY` when the owner set it, else `policy.toml` in the config directory. Never a path
/// from a job or the current directory.
pub fn policy_path() -> Result<PathBuf> {
    match std::env::var_os("RACK_POLICY").filter(|v| !v.is_empty()) {
        Some(p) => Ok(PathBuf::from(p)),
        None => Ok(crate::config::default_dir()?.join("policy.toml")),
    }
}

/// Reads and validates the policy at `path`; `state_dir` is where the active profile and audit
/// log live, which no profile may reach.
///
/// # Errors
///
/// Every problem found: unreadable file, loose permissions, bad TOML, unknown keys, bad values.
pub fn load(path: &Path, state_dir: &Path) -> std::result::Result<Policy, Problems> {
    let text =
        std::fs::read_to_string(path).map_err(|e| Problems(vec![format!("cannot read {}: {e}", path.display())]))?;
    let mut problems = Vec::new();
    if let Some(p) = loose_permissions(path) {
        problems.push(p);
    }
    let raw: RawPolicy = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            problems.push(format!("{}: {}", path.display(), e.message()));
            return Err(Problems(problems));
        }
    };
    let policy = validate(raw, path, state_dir, &mut problems);
    if problems.is_empty() {
        Ok(policy)
    } else {
        Err(Problems(problems))
    }
}

/// On Unix the file must be readable and writable by its owner only, since whoever can write it
/// decides what the worker runs. Windows ACLs are not checked (see SECURITY.md).
fn loose_permissions(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then(|| {
            format!(
                "{} has mode {mode:o}: only its owner may read or write it (chmod 600)",
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    worker: RawWorker,
    #[serde(default)]
    profiles: BTreeMap<String, RawProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorker {
    workdir: String,
    #[serde(default = "default_open")]
    max_open_jobs: u32,
    #[serde(default = "default_runtime")]
    max_runtime_secs: u64,
    #[serde(default = "default_output")]
    max_output_bytes: u64,
    #[serde(default = "default_hops")]
    max_hops: i64,
    #[serde(default = "default_accept")]
    accept_from: Vec<String>,
    #[serde(default = "default_no_profile")]
    no_profile: String,
}

fn default_open() -> u32 {
    1
}
fn default_runtime() -> u64 {
    3600
}
fn default_output() -> u64 {
    1 << 20
}
fn default_hops() -> i64 {
    1
}
fn default_accept() -> Vec<String> {
    vec!["main".to_owned()]
}
fn default_no_profile() -> String {
    "deny".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    description: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    allow_run: Vec<RawRule>,
    read: Option<Vec<String>>,
    write: Option<Vec<String>>,
    #[serde(default)]
    network: RawNetwork,
    #[serde(default)]
    env_allow: Vec<String>,
    timeout_secs: Option<u64>,
    max_output_bytes: Option<u64>,
    #[serde(default)]
    returns: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawNetwork {
    Flag(bool),
    Hosts(Vec<String>),
}

impl Default for RawNetwork {
    fn default() -> Self {
        RawNetwork::Flag(false)
    }
}

fn validate(raw: RawPolicy, path: &Path, state_dir: &Path, problems: &mut Vec<String>) -> Policy {
    if raw.version != VERSION {
        problems.push(format!(
            "version = {} is not supported; this rack reads version = {VERSION}",
            raw.version
        ));
    }
    let w = raw.worker;
    let workdir = match paths::resolve(Path::new(&w.workdir)) {
        Ok(r) if Path::new(&w.workdir).is_dir() => r,
        Ok(r) => {
            problems.push(format!("[worker] workdir `{}` is not an existing directory", w.workdir));
            r
        }
        Err(e) => {
            problems.push(format!("[worker] workdir: {e}"));
            Resolved::unchecked(&w.workdir)
        }
    };
    if w.workdir.contains(['{', '*', '?']) {
        problems.push("[worker] workdir may hold no placeholder or wildcard".to_owned());
    }
    let state = paths::resolve(state_dir).ok();
    let own = paths::resolve(path).ok();
    for (what, p) in [("the policy file", &own), ("the config directory", &state)] {
        if p.as_ref().is_some_and(|p| p.within(&workdir)) {
            problems.push(format!(
                "{what} is inside the workdir, where the worker could change it"
            ));
        }
    }
    if !(1..=64).contains(&w.max_open_jobs) {
        problems.push("[worker] max_open_jobs must be 1 to 64".to_owned());
    }
    if w.max_runtime_secs == 0 {
        problems.push("[worker] max_runtime_secs must be above 0".to_owned());
    }
    if !(0..=3).contains(&w.max_hops) {
        problems.push("[worker] max_hops must be 0 to 3 (the server stops chains at 3)".to_owned());
    }
    let mut accept_main = false;
    for role in &w.accept_from {
        match role.as_str() {
            "main" => accept_main = true,
            other => problems.push(format!(
                "[worker] accept_from: `{other}` cannot delegate; only `main` carries the owner's authority \
                 (decision #44), so the list is [\"main\"] or []"
            )),
        }
    }
    let no_profile = match w.no_profile.as_str() {
        "deny" => NoProfile::Deny,
        "ask" => NoProfile::Ask,
        other => {
            problems.push(format!("[worker] no_profile = \"{other}\": use \"deny\" or \"ask\""));
            NoProfile::Deny
        }
    };
    let mut profiles = BTreeMap::new();
    for (name, mut p) in raw.profiles {
        let label = format!("[profiles.{name}]");
        let name = match name.parse::<ProfileName>() {
            Ok(n) => n,
            Err(e) => {
                problems.push(format!("{label}: {e}"));
                continue;
            }
        };
        let vars = std::mem::take(&mut p.vars);
        let mut ctx = Ctx {
            label: &label,
            workdir: &w.workdir,
            vars: &vars,
            problems,
        };
        let profile = ctx.profile(p, &workdir, w.max_output_bytes, state.as_ref(), own.as_ref());
        profiles.insert(name, profile);
    }
    Policy {
        path: path.to_path_buf(),
        workdir,
        max_open_jobs: w.max_open_jobs,
        max_runtime_secs: w.max_runtime_secs,
        max_output_bytes: w.max_output_bytes,
        max_hops: w.max_hops,
        accept_main,
        no_profile,
        profiles,
    }
}

struct Ctx<'a> {
    label: &'a str,
    workdir: &'a str,
    vars: &'a BTreeMap<String, String>,
    problems: &'a mut Vec<String>,
}

impl Ctx<'_> {
    fn problem(&mut self, text: String) {
        self.problems.push(format!("{}: {text}", self.label));
    }

    /// Replaces `{workdir}` and `{VAR}` from the profile's `vars`.
    fn expand(&mut self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let Some(close) = rest[open..].find('}') else {
                self.problem(format!("`{text}` has an unclosed `{{`"));
                return None;
            };
            let name = &rest[open + 1..open + close];
            match (name, self.vars.get(name)) {
                ("workdir", _) => out.push_str(self.workdir),
                (_, Some(v)) if !v.contains(['{', '}']) => out.push_str(v),
                (_, Some(_)) => {
                    self.problem(format!("var `{name}` may not hold `{{` or `}}`"));
                    return None;
                }
                (_, None) => {
                    self.problem(format!(
                        "`{{{name}}}` in `{text}` is not `{{workdir}}` or a key of vars"
                    ));
                    return None;
                }
            }
            rest = &rest[open + close + 1..];
        }
        if rest.contains('}') {
            self.problem(format!("`{text}` has a stray `}}`"));
            return None;
        }
        out.push_str(rest);
        Some(out)
    }

    fn paths(&mut self, key: &str, entries: Option<Vec<String>>, default: &Resolved) -> Vec<Resolved> {
        let Some(entries) = entries else {
            return vec![default.clone()];
        };
        entries
            .iter()
            .filter_map(|e| {
                let expanded = self.expand(e)?;
                match paths::resolve_pattern(&expanded) {
                    Ok(r) => Some(r),
                    Err(err) => {
                        self.problem(format!("{key}: {err}"));
                        None
                    }
                }
            })
            .collect()
    }

    fn profile(
        &mut self,
        p: RawProfile,
        workdir: &Resolved,
        default_output: u64,
        state: Option<&Resolved>,
        own: Option<&Resolved>,
    ) -> Profile {
        if p.description.trim().is_empty() {
            self.problem("description is empty".to_owned());
        }
        for key in self.vars.keys() {
            if key == "workdir" || !shell_name(key) {
                self.problem(format!("vars: `{key}` must be [a-z_][a-z0-9_]* and not `workdir`"));
            }
        }
        let read = self.paths("read", p.read, workdir);
        let write = self.paths("write", p.write, workdir);
        for w in &write {
            for (what, target) in [("the config directory", state), ("the policy file", own)] {
                if target.is_some_and(|t| t.covered_by(w)) {
                    self.problem(format!(
                        "write `{}` reaches {what}: the worker could change its own policy",
                        w.as_str()
                    ));
                }
            }
        }
        let mut run = Vec::with_capacity(p.allow_run.len());
        for rule in p.allow_run {
            let Some(cmd) = self.expand(&rule.cmd) else {
                continue;
            };
            if !Path::new(&cmd).is_absolute() || glob::is_pattern(&cmd) {
                self.problem(format!(
                    "allow_run cmd `{cmd}` must be an absolute path without wildcards, so no PATH lookup decides what runs"
                ));
                continue;
            }
            if let Ok(real) = paths::resolve(Path::new(&cmd))
                && write.iter().any(|w| real.covered_by(w))
            {
                self.problem(format!(
                    "allow_run cmd `{cmd}` is inside a write path: the worker could replace it with anything"
                ));
            }
            let args: Option<Vec<String>> = rule.args.iter().map(|a| self.expand(a)).collect();
            if let Some(args) = args {
                run.push(RunRule { cmd, args });
            }
        }
        let network = match p.network {
            RawNetwork::Flag(false) => Vec::new(),
            RawNetwork::Flag(true) => {
                self.problem("network = true is not allowed: list the hosts, [\"host:port\", ...]".to_owned());
                Vec::new()
            }
            RawNetwork::Hosts(hosts) => {
                for h in &hosts {
                    if !host_port(h) {
                        self.problem(format!("network `{h}` is not host:port"));
                    }
                }
                hosts
            }
        };
        for name in &p.env_allow {
            if !shell_name_any_case(name) {
                self.problem(format!("env_allow `{name}` is not a variable name"));
            } else if FORBIDDEN_ENV.contains(&name.as_str())
                || FORBIDDEN_ENV_PREFIXES.iter().any(|pre| name.starts_with(pre))
            {
                self.problem(format!(
                    "env_allow `{name}` would let a command change which code runs or where rack reads its config"
                ));
            }
        }
        for r in &p.returns {
            let ok = match r.strip_prefix("files:") {
                Some(glob) => {
                    !glob.is_empty()
                        && !Path::new(glob).is_absolute()
                        && !glob.starts_with(['/', '\\'])
                        && !has_dotdot(glob)
                }
                None => matches!(r.as_str(), "stdout" | "stderr" | "exit_code"),
            };
            if !ok {
                self.problem(format!(
                    "returns `{r}`: use stdout, stderr, exit_code or files:<glob relative to the workdir>"
                ));
            }
        }
        if p.timeout_secs == Some(0) {
            self.problem("timeout_secs must be above 0".to_owned());
        }
        Profile {
            description: p.description,
            enabled: p.enabled,
            run,
            read,
            write,
            network,
            env_allow: p.env_allow,
            timeout_secs: p.timeout_secs,
            max_output_bytes: p.max_output_bytes.unwrap_or(default_output),
            returns: p.returns,
        }
    }
}

fn shell_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn shell_name_any_case(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn host_port(s: &str) -> bool {
    s.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && port.parse::<u16>().is_ok_and(|p| p > 0)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use super::*;

    /// A temp dir with a workdir and a config dir, cleaned up on drop.
    pub(crate) struct Fixture {
        pub root: PathBuf,
        pub work: PathBuf,
        pub conf: PathBuf,
    }

    impl Fixture {
        pub(crate) fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("rack-policy-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("work")).expect("work");
            fs::create_dir_all(root.join("conf")).expect("conf");
            let root = fs::canonicalize(&root).expect("canonical");
            Self {
                work: root.join("work"),
                conf: root.join("conf"),
                root,
            }
        }

        pub(crate) fn write(&self, text: &str) -> PathBuf {
            let path = self.conf.join("policy.toml");
            fs::write(&path, text.replace("@WORK@", &self.work.display().to_string())).expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
            }
            path
        }

        pub(crate) fn load(&self, text: &str) -> std::result::Result<Policy, Problems> {
            load(&self.write(text), &self.conf)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    pub(crate) const BASIC: &str = r#"
version = 1
[worker]
workdir = "@WORK@"
[profiles.workdir]
description = "inside the workdir"
enabled = true
allow_run = [ { cmd = "/usr/bin/true", args = ["--ok", "--n=*", "{workdir}/*"] } ]
[profiles.off]
description = "disabled"
"#;

    #[test]
    fn a_basic_policy_loads_with_defaults() {
        let f = Fixture::new("basic");
        let p = f.load(BASIC).expect("valid");
        assert_eq!(p.max_open_jobs, 1);
        assert_eq!(p.max_hops, 1);
        assert!(p.accept_main);
        assert_eq!(p.no_profile, NoProfile::Deny);
        let w = &p.profiles[&"workdir".parse().expect("name")];
        assert!(w.enabled);
        assert_eq!(w.read, std::slice::from_ref(&p.workdir));
        assert_eq!(w.write, std::slice::from_ref(&p.workdir));
        assert_eq!(w.run[0].args[2], format!("{}/*", f.work.display()));
        assert!(!p.profiles[&"off".parse().expect("name")].enabled);
    }

    #[test]
    fn the_template_is_valid_and_only_workdir_is_enabled() {
        let f = Fixture::new("template");
        let text = TEMPLATE.replace(TEMPLATE_WORKDIR, &format!("workdir = '{}'", f.work.display()));
        assert_ne!(text, TEMPLATE, "the template must hold the workdir line init replaces");
        let p = load(&f.write(&text), &f.conf).expect("template validates");
        let enabled: Vec<&str> = p
            .profiles
            .iter()
            .filter(|(_, p)| p.enabled)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(enabled, ["workdir"]);
        for name in ["app-test", "compat-launch", "diagnostics"] {
            assert!(p.profiles.contains_key(&name.parse().expect("name")), "{name}");
        }
    }

    #[test]
    fn every_problem_is_listed() {
        let f = Fixture::new("problems");
        let err = f
            .load(
                r#"
version = 2
[worker]
workdir = "relative/dir"
accept_from = ["main", "worker"]
no_profile = "maybe"
max_hops = 9
[profiles.BAD]
description = "x"
[profiles.p]
description = "x"
allow_run = [ { cmd = "true" }, { cmd = "/usr/bin/x", args = ["{nope}"] } ]
read = ["etc"]
network = true
env_allow = ["LD_PRELOAD", "RACK_POLICY", "WINEPREFIX"]
returns = ["files:../x", "stdout"]
"#,
            )
            .expect_err("invalid");
        let all = err.to_string();
        for needle in [
            "version = 2",
            "workdir",
            "`worker` cannot delegate",
            "no_profile",
            "max_hops",
            "`BAD`",
            "`true` must be an absolute path",
            "{nope}",
            "read: `etc`",
            "network = true",
            "LD_PRELOAD",
            "RACK_POLICY",
            "files:../x",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in {all}");
        }
        assert!(!all.contains("WINEPREFIX"), "{all}");
    }

    #[test]
    fn unknown_keys_and_bad_toml_fail() {
        let f = Fixture::new("unknown");
        let err = f
            .load("version = 1\n[worker]\nworkdir = \"@WORK@\"\nshell = true\n")
            .expect_err("unknown key");
        assert!(err.to_string().contains("shell"), "{err}");
        assert!(f.load("version = ").is_err());
        assert!(load(&f.conf.join("missing.toml"), &f.conf).is_err());
    }

    #[test]
    fn a_command_the_worker_can_write_is_refused() {
        let f = Fixture::new("writable-cmd");
        let err = f
            .load(
                "version = 1\n[worker]\nworkdir = \"@WORK@\"\n[profiles.p]\ndescription = \"x\"\n\
                 allow_run = [ { cmd = \"{workdir}/run.sh\" } ]\n",
            )
            .expect_err("writable");
        assert!(err.to_string().contains("inside a write path"), "{err}");
    }

    #[test]
    fn the_worker_may_not_write_its_own_policy() {
        let f = Fixture::new("own-policy");
        let text = format!(
            "version = 1\n[worker]\nworkdir = \"@WORK@\"\n[profiles.p]\ndescription = \"x\"\nwrite = [\"{}\"]\n",
            f.root.display()
        );
        let err = f.load(&text).expect_err("reaches config");
        assert!(err.to_string().contains("could change its own policy"), "{err}");
        let inside = Fixture::new("policy-in-work");
        let err = load(&inside.write(BASIC), &inside.work).expect_err("config dir in workdir");
        assert!(err.to_string().contains("inside the workdir"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_policy_others_can_read_or_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("mode");
        let path = f.write(BASIC);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = load(&path, &f.conf).expect_err("mode 644");
        assert!(err.to_string().contains("chmod 600"), "{err}");
    }

    #[test]
    fn argv_matching_is_anchored_and_literal() {
        let rule = RunRule {
            cmd: "/usr/bin/true".to_owned(),
            args: vec!["--ok".to_owned(), "--n=*".to_owned()],
        };
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(rule.matches(&argv(&["/usr/bin/true", "--ok", "--n=3"])));
        assert!(
            !rule.matches(&argv(&["true", "--ok", "--n=3"])),
            "program by absolute path only"
        );
        assert!(
            !rule.matches(&argv(&["/usr/bin/true", "--okay", "--n=3"])),
            "no prefix trick"
        );
        assert!(
            !rule.matches(&argv(&["/usr/bin/true", "--ok"])),
            "same number of arguments"
        );
        assert!(!rule.matches(&argv(&["/usr/bin/true", "--ok", "--n=3", "--x"])));
        assert!(
            rule.matches(&argv(&["/usr/bin/true", "--ok", "--n=3; rm -rf /"])),
            "a literal argument: no shell ever sees it"
        );
        let paths = RunRule {
            cmd: "/usr/bin/cat".to_owned(),
            args: vec!["/w/*".to_owned()],
        };
        assert!(paths.matches(&argv(&["/usr/bin/cat", "/w/a.txt"])));
        assert!(
            !paths.matches(&argv(&["/usr/bin/cat", "/w/../etc/passwd"])),
            "no `..` via a wildcard"
        );
    }
}
