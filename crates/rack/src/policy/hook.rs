//! `rack policy hook`: a Claude Code `PreToolUse` command hook that holds a worker's tool calls to
//! its active profile. Contract (input fields, `hookSpecificOutput.permissionDecision`, the Bash,
//! Read, Edit, Write, Glob and Grep `tool_input` fields): the official hooks reference,
//! <https://code.claude.com/docs/en/hooks.md#pretooluse> and
//! <https://code.claude.com/docs/en/hooks.md#pretooluse-decision-control>.
//!
//! It only answers; it never runs what it judges. Anything it cannot read is denied.

use std::path::Path;

use serde_json::{Value, json};

use super::journal::Active;
use super::paths;
use super::{NoProfile, Policy, Problems, Profile, shell};

/// Tools that act only inside the session (questions to the owner, plans, the to-do list): the
/// hook leaves them to Claude Code's own permissions.
const SESSION_TOOLS: &[&str] = &["AskUserQuestion", "ExitPlanMode", "TodoWrite"];

/// `rack` subcommands a worker needs to take, do and answer a task. Everything else (`send`,
/// `update`, `admin`, `link`, `policy init`...) is the owner's.
const RACK_PROTOCOL: &[&str] = &[
    "whoami",
    "agents",
    "inbox",
    "wait",
    "show",
    "accept",
    "reject",
    "result",
    "attach",
    "get-attachment",
    "say",
    "notify",
    "report",
    "links",
    "guide",
    "help",
    "version",
    "--version",
    "-V",
    "policy",
];
const RACK_POLICY_SUBCOMMANDS: &[&str] = &["decide", "show", "check", "release"];
/// `racking-mcp` tools with the same scope, minus those that move files.
const RACKING_MCP_TOOLS: &[&str] = &[
    "racking_whoami",
    "racking_agents",
    "racking_inbox",
    "racking_wait",
    "racking_show",
    "racking_accept",
    "racking_reject",
    "racking_guide",
];

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow {
        reason: String,
        updated_input: Option<Value>,
    },
    Deny(String),
    Ask(String),
    /// No opinion: Claude Code's own permissions decide.
    Abstain,
}

impl Verdict {
    fn allow(reason: impl Into<String>) -> Self {
        Verdict::Allow {
            reason: reason.into(),
            updated_input: None,
        }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Allow { .. } => "allow",
            Verdict::Deny(_) => "deny",
            Verdict::Ask(_) => "ask",
            Verdict::Abstain => "abstain",
        }
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Verdict::Allow { reason, .. } | Verdict::Deny(reason) | Verdict::Ask(reason) => reason,
            Verdict::Abstain => "",
        }
    }

    /// The JSON Claude Code reads on stdout, or nothing for `Abstain`.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        if *self == Verdict::Abstain {
            return None;
        }
        let mut out = json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": self.name(),
            "permissionDecisionReason": format!("rack policy: {}", self.reason()),
        });
        if let Verdict::Allow {
            updated_input: Some(input),
            ..
        } = self
        {
            out["updatedInput"] = input.clone();
        }
        Some(json!({ "hookSpecificOutput": out }).to_string())
    }
}

/// What the hook knows besides the call: the policy, the active profile, and this binary's path.
pub struct Context<'a> {
    pub policy: Result<&'a Policy, &'a Problems>,
    pub active: Result<Option<&'a Active>, &'a str>,
    /// The running `rack`, which a worker may call by its full path.
    pub rack_exe: Option<&'a str>,
}

/// The verdict and the tool it was about, for the audit log.
pub struct Judged {
    pub verdict: Verdict,
    pub tool: Option<String>,
}

/// The tool call as the hooks reference documents it.
struct Call<'a> {
    tool: &'a str,
    input: &'a serde_json::Map<String, Value>,
    cwd: &'a str,
}

fn call(input: &Value) -> Result<Call<'_>, String> {
    if input["hook_event_name"] != "PreToolUse" {
        return Err("the input is not a PreToolUse event".to_owned());
    }
    let tool = input["tool_name"].as_str().ok_or("tool_name is missing")?;
    let tool_input = input["tool_input"].as_object().ok_or("tool_input is missing")?;
    let cwd = input["cwd"].as_str().ok_or("cwd is missing")?;
    Ok(Call {
        tool,
        input: tool_input,
        cwd,
    })
}

/// Judges one hook input (the JSON Claude Code writes on stdin).
#[must_use]
pub fn evaluate(stdin: &str, ctx: &Context) -> Judged {
    let value: Value = match serde_json::from_str(stdin) {
        Ok(v) => v,
        Err(e) => {
            return Judged {
                verdict: Verdict::Deny(format!("unreadable hook input ({e})")),
                tool: None,
            };
        }
    };
    match call(&value) {
        Ok(c) => Judged {
            verdict: judge(&c, ctx),
            tool: Some(c.tool.to_owned()),
        },
        Err(e) => Judged {
            verdict: Verdict::Deny(format!("unreadable hook input: {e}")),
            tool: value["tool_name"].as_str().map(str::to_owned),
        },
    }
}

/// The active profile, or the verdict for a call that needs one when there is none.
struct Scope<'a> {
    policy: &'a Policy,
    active: Option<(&'a str, &'a Profile)>,
}

impl Scope<'_> {
    fn outside(&self, what: &str) -> Verdict {
        match self.active {
            Some((name, _)) => Verdict::Deny(format!("{what}: not allowed by the active profile {name}")),
            None => {
                let text = format!(
                    "{what}: no profile is active (a task from main must pass `rack policy decide JOB --accept` first)"
                );
                match self.policy.no_profile {
                    NoProfile::Deny => Verdict::Deny(text),
                    NoProfile::Ask => Verdict::Ask(text),
                }
            }
        }
    }
}

fn judge(c: &Call, ctx: &Context) -> Verdict {
    if SESSION_TOOLS.contains(&c.tool) {
        return Verdict::Abstain;
    }
    let policy = match ctx.policy {
        Ok(p) => p,
        Err(problems) => return Verdict::Deny(format!("the local policy is unusable: {problems}")),
    };
    let active = match ctx.active {
        Ok(a) => a,
        Err(e) => return Verdict::Deny(e.to_owned()),
    };
    let active = match active {
        None => None,
        Some(a) => match a.profile.parse().ok().and_then(|n| policy.profiles.get_key_value(&n)) {
            Some((name, p)) if p.enabled => Some((name.as_str(), p)),
            _ => {
                return Verdict::Deny(format!(
                    "the active profile {} is no longer enabled in the local policy",
                    a.profile
                ));
            }
        },
    };
    let scope = Scope { policy, active };
    if let Some(tool) = c.tool.strip_prefix("mcp__") {
        return match tool.rsplit_once("__") {
            Some((_, name)) if RACKING_MCP_TOOLS.contains(&name) => Verdict::allow("Racking protocol tool"),
            _ => scope.outside(&format!("the tool {}", c.tool)),
        };
    }
    let field = |name: &str| c.input.get(name).and_then(Value::as_str);
    match c.tool {
        "Bash" => bash(c, &scope, ctx.rack_exe),
        "Read" => file(&scope, field("file_path"), Access::Read),
        "Edit" | "Write" => file(&scope, field("file_path"), Access::Write),
        "NotebookEdit" => file(&scope, field("notebook_path"), Access::Write),
        "Glob" | "Grep" => {
            for pattern in [field("pattern").filter(|_| c.tool == "Glob"), field("glob")]
                .into_iter()
                .flatten()
            {
                if Path::new(pattern).is_absolute() || pattern.starts_with('/') || pattern.contains("..") {
                    return Verdict::Deny(format!("glob `{pattern}` must stay under the searched path"));
                }
            }
            file(&scope, Some(field("path").unwrap_or(c.cwd)), Access::Read)
        }
        "WebFetch" => web_fetch(&scope, field("url")),
        other => scope.outside(&format!("the tool {other}")),
    }
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
}

fn file(scope: &Scope, path: Option<&str>, access: Access) -> Verdict {
    let Some(path) = path else {
        return Verdict::Deny("the tool input names no path".to_owned());
    };
    let Some((name, profile)) = scope.active else {
        return scope.outside(&format!("access to {path}"));
    };
    let resolved = match paths::resolve(Path::new(path)) {
        Ok(r) => r,
        Err(e) => return Verdict::Deny(e),
    };
    let (ok, verb) = match access {
        Access::Read => (profile.may_read(&resolved), "read"),
        Access::Write => (profile.may_write(&resolved), "write"),
    };
    if ok {
        Verdict::allow(format!("profile {name} may {verb} {}", resolved.as_str()))
    } else {
        Verdict::Deny(format!(
            "profile {name} may not {verb} {}: outside its {verb} paths",
            resolved.as_str()
        ))
    }
}

fn bash(c: &Call, scope: &Scope, rack_exe: Option<&str>) -> Verdict {
    let Some(command) = c.input.get("command").and_then(Value::as_str) else {
        return Verdict::Deny("the Bash call has no command".to_owned());
    };
    if c.input.get("run_in_background") == Some(&Value::Bool(true)) {
        return Verdict::Deny("background commands escape the profile's timeout; run it in the foreground".to_owned());
    }
    let argv = match shell::parse(command) {
        Ok(a) => a,
        Err(e) => return Verdict::Deny(e),
    };
    let program = argv.args[0].as_str();
    if program == "rack" || program == "rack.exe" || Some(program) == rack_exe {
        if !argv.env.is_empty() {
            return Verdict::Deny("rack runs without variable assignments in front".to_owned());
        }
        return rack(&argv.args[1..], c.cwd, scope);
    }
    let Some((name, profile)) = scope.active else {
        return scope.outside(&format!("running {program}"));
    };
    if let Some((var, _)) = argv.env.iter().find(|(var, _)| !profile.env_allow.contains(var)) {
        return Verdict::Deny(format!("profile {name} does not let a command set {var}"));
    }
    match paths::resolve(Path::new(c.cwd)) {
        Ok(cwd) if cwd.within(&scope.policy.workdir) => {}
        _ => {
            return Verdict::Deny(format!(
                "commands of a profile run from inside the workdir {}, not from {}",
                scope.policy.workdir.as_str(),
                c.cwd
            ));
        }
    }
    let Some(rule) = profile.run.iter().find(|r| r.matches(&argv.args)) else {
        return Verdict::Deny(format!(
            "`{program}` with these arguments is not in profile {name}'s allow_run (see rack policy show)"
        ));
    };
    let updated_input = profile.timeout_secs.map(|secs| {
        let limit = secs.saturating_mul(1000);
        let asked = c.input.get("timeout").and_then(Value::as_u64).unwrap_or(limit);
        let mut input = c.input.clone();
        input.insert("timeout".to_owned(), json!(asked.min(limit)));
        Value::Object(input)
    });
    Verdict::Allow {
        reason: format!("profile {name} allows `{}`", rule.display()),
        updated_input,
    }
}

/// A `rack` call: allowed when it is part of the protocol, and when every file it would send or
/// save stays inside the active profile.
fn rack(args: &[String], cwd: &str, scope: &Scope) -> Verdict {
    let Some(sub) = args.first().map(String::as_str) else {
        return Verdict::allow("rack usage text");
    };
    if !RACK_PROTOCOL.contains(&sub) {
        return Verdict::Deny(format!("`rack {sub}` is the owner's, not part of a worker's protocol"));
    }
    if sub == "policy"
        && !args
            .get(1)
            .is_some_and(|s| RACK_POLICY_SUBCOMMANDS.contains(&s.as_str()))
    {
        return Verdict::Deny("of rack policy, a worker runs decide, show, check and release only".to_owned());
    }
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut positional = Vec::new();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let value = |it: &mut std::slice::Iter<String>| inline.clone().or_else(|| it.next().cloned());
        match name {
            "--state" => return Verdict::Deny("rack --state is the owner's".to_owned()),
            "--attach" | "--file" => reads.extend(value(&mut it)),
            "--out" => writes.extend(value(&mut it)),
            n if n.starts_with('-') => {}
            _ => positional.push(a.clone()),
        }
    }
    match sub {
        // `rack attach JOB FILE...` sends the files.
        "attach" => reads.extend(positional.iter().skip(1).cloned()),
        // Without --out it saves NAME in the current directory.
        "get-attachment" if writes.is_empty() => writes.extend(positional.get(1).cloned()),
        _ => {}
    }
    for (files, access) in [(reads, Access::Read), (writes, Access::Write)] {
        for f in files {
            let full = Path::new(cwd).join(&f);
            let v = file(scope, full.to_str(), access);
            if !matches!(v, Verdict::Allow { .. }) {
                return v;
            }
        }
    }
    Verdict::allow(format!("rack {sub} is part of the protocol"))
}

fn web_fetch(scope: &Scope, url: Option<&str>) -> Verdict {
    let Some(url) = url else {
        return Verdict::Deny("WebFetch without a url".to_owned());
    };
    let Some((name, profile)) = scope.active else {
        return scope.outside(&format!("fetching {url}"));
    };
    match host_port(url) {
        Some(hp) if profile.network.iter().any(|n| n.eq_ignore_ascii_case(&hp)) => {
            Verdict::allow(format!("profile {name} may reach {hp}"))
        }
        Some(hp) => Verdict::Deny(format!("profile {name} may not reach {hp}")),
        None => Verdict::Deny(format!("cannot tell which host `{url}` reaches")),
    }
}

/// `host:port` of an http(s) URL, the port filled in from the scheme.
fn host_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') || authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some(format!("{host}:{}", port.parse::<u16>().ok()?)),
        None => Some(format!("{authority}:{default}")),
    }
}

/// The `--settings` file for a worker's Claude Code session: this hook on every tool call, allow
/// rules for the exact commands of enabled profiles, and the owner's choice to accept messages
/// from their other sessions in this session only. Keys: <https://code.claude.com/docs/en/settings-reference.md>
/// (`hooks`, `permissions`, `crossSessionInbound`) and <https://code.claude.com/docs/en/permissions.md>.
#[must_use]
pub fn settings(policy: &Policy, rack_exe: &str) -> Value {
    let allow: Vec<String> = policy
        .profiles
        .values()
        .filter(|p| p.enabled)
        .flat_map(|p| &p.run)
        // A rule with a wildcard is wider in Claude Code's syntax than in ours; the hook allows those.
        .filter(|r| !r.args.iter().any(|a| super::glob::is_pattern(a)))
        .map(|r| format!("Bash({})", r.display()))
        .collect();
    let mut deny = vec!["Bash(sudo *)".to_owned(), "WebSearch".to_owned()];
    if policy.profiles.values().all(|p| !p.enabled || p.network.is_empty()) {
        deny.push("WebFetch".to_owned());
    }
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": rack_exe,
                    "args": ["policy", "hook"],
                    "timeout": 30
                }]
            }]
        },
        "permissions": { "allow": allow, "deny": deny },
        "crossSessionInbound": "accept"
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::policy::tests::Fixture;

    const POLICY: &str = r#"
version = 1
[worker]
workdir = "@WORK@"
[profiles.workdir]
description = "inside the workdir"
enabled = true
allow_run = [ { cmd = "/usr/bin/true", args = ["--ok", "--n=*"] } ]
env_allow = ["WINEDEBUG"]
timeout_secs = 60
network = ["example.org:443"]
"#;

    struct Hook {
        f: Fixture,
        policy: Policy,
    }

    impl Hook {
        fn new(name: &str, text: &str) -> Self {
            let f = Fixture::new(name);
            let policy = f.load(text).expect("policy");
            Self { f, policy }
        }

        fn active(&self) -> Active {
            Active {
                job: "j1".to_owned(),
                profile: "workdir".to_owned(),
                from: "linux".to_owned(),
                accepted_at: 0,
                expires_at: u64::MAX,
            }
        }

        fn run(&self, active: Option<&Active>, tool: &str, input: Value) -> Verdict {
            self.run_in(active, tool, input, &self.f.work)
        }

        fn run_in(&self, active: Option<&Active>, tool: &str, input: Value, cwd: &Path) -> Verdict {
            // The shape of the PreToolUse sample in the hooks reference.
            let stdin = json!({
                "session_id": "abc123",
                "transcript_path": "/home/user/.claude/projects/x/transcript.jsonl",
                "cwd": cwd,
                "permission_mode": "default",
                "hook_event_name": "PreToolUse",
                "tool_name": tool,
                "tool_input": input,
                "tool_use_id": "toolu_01ABC123"
            });
            let ctx = Context {
                policy: Ok(&self.policy),
                active: Ok(active),
                rack_exe: Some("/opt/rack/rack"),
            };
            evaluate(&stdin.to_string(), &ctx).verdict
        }

        fn bash(&self, active: Option<&Active>, command: &str) -> Verdict {
            self.run(
                active,
                "Bash",
                json!({"command": command, "description": "x", "timeout": 120_000, "run_in_background": false}),
            )
        }
    }

    fn allowed(v: &Verdict) -> bool {
        matches!(v, Verdict::Allow { .. })
    }

    #[test]
    fn bash_inside_the_profile_is_allowed_with_its_timeout() {
        let h = Hook::new("hook-bash", POLICY);
        let a = h.active();
        let v = h.bash(Some(&a), "/usr/bin/true --ok --n=3");
        let Verdict::Allow {
            updated_input: Some(input),
            ..
        } = &v
        else {
            panic!("{v:?}");
        };
        assert_eq!(input["timeout"], 60_000);
        assert_eq!(input["command"], "/usr/bin/true --ok --n=3");
        assert!(allowed(&h.bash(Some(&a), "WINEDEBUG=-all /usr/bin/true --ok --n=")));
    }

    #[test]
    fn bash_outside_the_profile_is_denied() {
        let h = Hook::new("hook-bash-deny", POLICY);
        let a = h.active();
        for cmd in [
            "/usr/bin/true --ok --n=3 --more",
            "/usr/bin/true --okay --n=3",
            "true --ok --n=3",
            "/usr/bin/true --ok --n=3; rm -rf /",
            "/usr/bin/true --ok --n=$(id)",
            "LD_PRELOAD=/x.so /usr/bin/true --ok --n=3",
            "rack send linux x",
            "rack update",
            "rack policy init",
            "rack --state /tmp/s inbox",
            "RACK_POLICY=/tmp/p rack policy decide j1",
        ] {
            assert!(matches!(h.bash(Some(&a), cmd), Verdict::Deny(_)), "{cmd}");
        }
        let background = h.run(
            Some(&a),
            "Bash",
            json!({"command": "/usr/bin/true --ok --n=1", "run_in_background": true}),
        );
        assert!(matches!(background, Verdict::Deny(_)));
        let elsewhere = h.run_in(
            Some(&a),
            "Bash",
            json!({"command": "/usr/bin/true --ok --n=1"}),
            &h.f.root,
        );
        assert!(matches!(elsewhere, Verdict::Deny(r) if r.contains("from inside the workdir")));
    }

    #[test]
    fn no_active_profile_denies_or_asks_but_rack_still_works() {
        let h = Hook::new("hook-none", POLICY);
        assert!(matches!(h.bash(None, "/usr/bin/true --ok --n=3"), Verdict::Deny(r) if r.contains("no profile")));
        assert!(allowed(&h.bash(None, "rack show j1")));
        assert!(allowed(&h.bash(None, "/opt/rack/rack policy decide j1 --accept")));
        assert!(matches!(
            h.bash(None, &format!("rack result j1 --attach {}/out.txt", h.f.work.display())),
            Verdict::Deny(_)
        ));
        let ask = Hook::new(
            "hook-ask",
            &POLICY.replace("[worker]\n", "[worker]\nno_profile = \"ask\"\n"),
        );
        assert!(matches!(ask.bash(None, "/usr/bin/true --ok --n=3"), Verdict::Ask(_)));
    }

    #[test]
    fn file_tools_follow_read_and_write_paths() {
        let h = Hook::new("hook-files", POLICY);
        let a = h.active();
        let inside = h.f.work.join("a.txt");
        let outside = h.f.root.join("secret.txt");
        fs::write(&outside, "x").expect("secret");
        assert!(allowed(&h.run(Some(&a), "Read", json!({"file_path": inside}))));
        assert!(allowed(&h.run(
            Some(&a),
            "Write",
            json!({"file_path": inside, "content": "x"})
        )));
        assert!(matches!(
            h.run(Some(&a), "Read", json!({"file_path": outside})),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            h.run(
                Some(&a),
                "Edit",
                json!({"file_path": h.f.work.join("../secret.txt"), "old_string": "x", "new_string": "y"})
            ),
            Verdict::Deny(_)
        ));
        assert!(allowed(&h.run(Some(&a), "Glob", json!({"pattern": "**/*.rs"}))));
        assert!(matches!(
            h.run(Some(&a), "Glob", json!({"pattern": "../*", "path": h.f.work})),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            h.run(Some(&a), "Grep", json!({"pattern": "x", "path": h.f.root})),
            Verdict::Deny(_)
        ));
        assert!(allowed(
            &h.bash(Some(&a), &format!("rack result j1 --attach {}", inside.display()))
        ));
        assert!(matches!(
            h.bash(Some(&a), &format!("rack attach j1 {}", outside.display())),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            h.bash(Some(&a), "rack get-attachment j1 x.bin --out /etc/x.bin"),
            Verdict::Deny(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workdir_is_denied() {
        let h = Hook::new("hook-link", POLICY);
        let a = h.active();
        std::os::unix::fs::symlink(&h.f.root, h.f.work.join("up")).expect("link");
        assert!(matches!(
            h.run(
                Some(&a),
                "Read",
                json!({"file_path": h.f.work.join("up/conf/policy.toml")})
            ),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn other_tools_network_and_mcp() {
        let h = Hook::new("hook-tools", POLICY);
        let a = h.active();
        assert!(matches!(h.run(Some(&a), "Agent", json!({})), Verdict::Deny(_)));
        assert_eq!(h.run(None, "TodoWrite", json!({"todos": []})), Verdict::Abstain);
        assert!(allowed(&h.run(
            Some(&a),
            "WebFetch",
            json!({"url": "https://example.org/x", "prompt": "p"})
        )));
        assert!(matches!(
            h.run(Some(&a), "WebFetch", json!({"url": "https://evil.example/x"})),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            h.run(
                Some(&a),
                "WebFetch",
                json!({"url": "https://example.org@evil.example/"})
            ),
            Verdict::Deny(_)
        ));
        assert!(allowed(&h.run(
            None,
            "mcp__racking__racking_show",
            json!({"job": "j1"})
        )));
        assert!(matches!(
            h.run(Some(&a), "mcp__racking__racking_send", json!({})),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn unreadable_input_bad_policy_and_stale_profile_deny() {
        let h = Hook::new("hook-bad", POLICY);
        let ctx = Context {
            policy: Ok(&h.policy),
            active: Ok(None),
            rack_exe: None,
        };
        for stdin in [
            "",
            "not json",
            "{}",
            r#"{"hook_event_name": "PostToolUse", "tool_name": "Bash"}"#,
        ] {
            assert!(matches!(evaluate(stdin, &ctx).verdict, Verdict::Deny(_)), "{stdin}");
        }
        let problems = Problems(vec!["bad".to_owned()]);
        let broken = Context {
            policy: Err(&problems),
            active: Ok(None),
            rack_exe: None,
        };
        let input = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "cwd": "/",
            "tool_input": {"command": "rack show j1"}});
        assert!(matches!(
            evaluate(&input.to_string(), &broken).verdict,
            Verdict::Deny(_)
        ));
        let stale = Active {
            profile: "gone".to_owned(),
            ..h.active()
        };
        assert!(matches!(h.bash(Some(&stale), "rack show j1"), Verdict::Deny(r) if r.contains("no longer")));
    }

    #[test]
    fn rendered_output_follows_the_documented_shape() {
        let out: Value = serde_json::from_str(&Verdict::Deny("no".to_owned()).render().expect("json")).expect("json");
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], "rack policy: no");
        assert_eq!(Verdict::Abstain.render(), None);
    }

    #[test]
    fn settings_register_the_hook_and_exact_rules() {
        let h = Hook::new(
            "hook-settings",
            &POLICY.replace("allow_run = [", "allow_run = [ { cmd = \"/usr/bin/uptime\" }, "),
        );
        let s = settings(&h.policy, "/opt/rack/rack");
        assert_eq!(
            s["hooks"]["PreToolUse"][0]["hooks"][0]["args"],
            json!(["policy", "hook"])
        );
        assert_eq!(s["permissions"]["allow"], json!(["Bash(/usr/bin/uptime)"]));
        assert_eq!(s["crossSessionInbound"], "accept");
        assert!(
            !s["permissions"]["deny"]
                .as_array()
                .expect("deny")
                .contains(&json!("WebFetch"))
        );
    }

    #[test]
    fn host_port_of_urls() {
        assert_eq!(host_port("https://a.b/x").as_deref(), Some("a.b:443"));
        assert_eq!(host_port("http://a.b:8080?x").as_deref(), Some("a.b:8080"));
        assert_eq!(host_port("ftp://a.b/"), None);
        assert_eq!(host_port("https://u@a.b/"), None);
    }
}
