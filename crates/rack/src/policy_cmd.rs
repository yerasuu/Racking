//! `rack policy`: the command-line side of the worker's local policy. The rules live in
//! `rack::policy`; this module reads files, asks the server and prints.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use rack::client::{Client, Job, Listing, RackError, Result, job_id, local};
use rack::config;
use rack::policy::decide::{self, Decision, Deny, JobFacts};
use rack::policy::hook::{self, Context, Verdict};
use rack::policy::journal::{self, Active, Entry};
use rack::policy::{self, Policy, Problems};

/// The denial `rack policy decide` exits with: 1, after the `deny` line on stdout.
fn denied() -> RackError {
    RackError::Local("denied by the local policy".to_owned())
}

struct Files {
    policy: PathBuf,
    dir: PathBuf,
}

impl Files {
    fn locate() -> Result<Self> {
        Ok(Self {
            policy: policy::policy_path()?,
            dir: config::default_dir()?,
        })
    }

    fn load(&self) -> std::result::Result<Policy, Problems> {
        policy::load(&self.policy, &self.dir)
    }

    fn audit(&self, entry: &Entry) -> std::result::Result<(), String> {
        journal::audit(&journal::audit_path(&self.dir), entry)
    }
}

/// Prints every problem and fails, for the commands that need a valid policy to say anything.
fn require(files: &Files) -> Result<Policy> {
    files.load().map_err(|problems| {
        for p in &problems.0 {
            println!("problem: {p}");
        }
        RackError::Local(format!(
            "{} has {} problem(s); nothing is allowed until it validates",
            files.policy.display(),
            problems.0.len()
        ))
    })
}

pub fn init(workdir: Option<&str>) -> Result<()> {
    let files = Files::locate()?;
    let workdir = match workdir {
        Some(w) => PathBuf::from(w),
        None => default_workdir()?,
    };
    if !workdir.is_absolute() {
        return Err(RackError::Usage("--workdir must be an absolute path".to_owned()));
    }
    let shown = workdir.display().to_string();
    // The template holds the workdir as a TOML literal string, which cannot contain a quote.
    if shown.contains('\'') {
        return Err(RackError::Usage("--workdir may not contain `'`".to_owned()));
    }
    let text = policy::TEMPLATE.replace(policy::TEMPLATE_WORKDIR, &format!("workdir = '{shown}'"));
    if let Some(dir) = files.policy.parent() {
        fs::create_dir_all(dir).map_err(|e| local(&format!("create {}", dir.display()), e))?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(&files.policy) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(RackError::Local(format!(
                "{} exists; rack never overwrites a policy: edit it, or move it away first",
                files.policy.display()
            )));
        }
        Err(e) => return Err(local(&format!("create {}", files.policy.display()), e)),
    };
    file.write_all(text.as_bytes())
        .map_err(|e| local(&format!("write {}", files.policy.display()), e))?;
    fs::create_dir_all(&workdir).map_err(|e| local(&format!("create {shown}"), e))?;
    println!(
        "wrote {} (workdir {shown}); only the workdir profile is enabled. Read it, then run rack policy check",
        files.policy.display()
    );
    Ok(())
}

fn default_workdir() -> Result<PathBuf> {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home)
        .map(|h| PathBuf::from(h).join("rack-work"))
        .ok_or_else(|| RackError::Usage(format!("{home} is not set; pass --workdir DIR")))
}

pub fn check() -> Result<()> {
    let files = Files::locate()?;
    let p = require(&files)?;
    let enabled = p.profiles.values().filter(|p| p.enabled).count();
    println!(
        "ok: {} is valid, {} profile(s), {enabled} enabled",
        files.policy.display(),
        p.profiles.len()
    );
    Ok(())
}

pub fn show() -> Result<()> {
    let files = Files::locate()?;
    let p = require(&files)?;
    println!("policy: {}", files.policy.display());
    println!("workdir: {}", p.workdir.as_str());
    println!(
        "worker: max_open_jobs {}, max_runtime_secs {}, max_output_bytes {}, max_hops {}, accept_from {}, no_profile {}",
        p.max_open_jobs,
        p.max_runtime_secs,
        p.max_output_bytes,
        p.max_hops,
        if p.accept_main { "[main]" } else { "[] (delegation off)" },
        match p.no_profile {
            policy::NoProfile::Deny => "deny",
            policy::NoProfile::Ask => "ask",
        }
    );
    for (name, prof) in &p.profiles {
        let state = if prof.enabled { "enabled" } else { "disabled" };
        println!("\nprofile {name} ({state}): {}", prof.description);
        for rule in &prof.run {
            println!("  run: {}", rule.display());
        }
        let list = |items: Vec<&str>| {
            if items.is_empty() {
                "none".to_owned()
            } else {
                items.join(", ")
            }
        };
        println!("  read: {}", list(prof.read.iter().map(|r| r.as_str()).collect()));
        println!("  write: {}", list(prof.write.iter().map(|r| r.as_str()).collect()));
        println!("  network: {}", list(prof.network.iter().map(String::as_str).collect()));
        if !prof.env_allow.is_empty() {
            println!("  env: {}", prof.env_allow.join(", "));
        }
        if let Some(t) = prof.timeout_secs {
            println!("  timeout: {t} s");
        }
        println!("  max output: {} bytes", prof.max_output_bytes);
        println!("  returns: {}", list(prof.returns.iter().map(String::as_str).collect()));
    }
    match journal::read_active(&journal::active_path(&files.dir), journal::now()) {
        Ok(Some(a)) => println!(
            "\nactive: profile {} for {} from {}, {} s left",
            a.profile,
            a.job,
            a.from,
            a.expires_at.saturating_sub(journal::now())
        ),
        Ok(None) => println!("\nactive: none"),
        Err(e) => println!("\nactive: {e}"),
    }
    Ok(())
}

/// What `decide` learned from the server, kept for the audit line.
struct Seen {
    job: Job,
    me: String,
    paused: bool,
    open_tasks: usize,
}

fn gather(c: &Client, id: &str) -> Result<Seen> {
    let me: Value = c.get("/whoami")?;
    let job: Job = c.get(&format!("/jobs/{id}"))?;
    let open: Listing = c.get("/inbox?since=0&state=accepted&limit=200")?;
    let open_tasks = open.jobs.iter().filter(|j| j.kind == "task" && j.id != job.id).count();
    Ok(Seen {
        me: me["agent"].as_str().unwrap_or_default().to_owned(),
        paused: me["paused"] == true || open.paused,
        job,
        open_tasks,
    })
}

pub fn decide(job: &str, accept: bool, connect: impl FnOnce() -> Result<Client>) -> Result<()> {
    let id = job_id(job)?;
    let files = Files::locate()?;
    let loaded = files.load();
    let now = journal::now();
    let active_path = journal::active_path(&files.dir);
    let mut entry = Entry {
        source: "decide",
        decision: "deny",
        job: Some(id),
        profile: None,
        from: None,
        from_role: None,
        tool: None,
        reason: "",
    };
    let deny = |entry: &Entry, reason: &str| -> Result<()> {
        println!("deny reason={reason}");
        let entry = Entry {
            decision: "deny",
            reason,
            ..*entry
        };
        if let Err(e) = files.audit(&entry) {
            eprintln!("rack: {e}");
        }
        Err(denied())
    };
    let policy = match &loaded {
        Ok(p) => p,
        Err(problems) => return deny(&entry, &Deny::Policy(problems.to_string()).to_string()),
    };
    let active = match journal::read_active(&active_path, now) {
        Ok(a) => a,
        Err(e) => return deny(&entry, &e),
    };
    let c = connect()?;
    let seen = match gather(&c, id) {
        Ok(s) => s,
        Err(e) => {
            deny(&entry, &format!("cannot read the job from the server: {e}")).ok();
            return Err(e);
        }
    };
    let facts = JobFacts {
        addressed_to_me: seen.job.to == seen.me,
        from_role: seen.job.from_role.as_deref(),
        kind: &seen.job.kind,
        state: &seen.job.state,
        hops: seen.job.hops,
        profile: &seen.job.profile,
        body: seen.job.body.as_deref().unwrap_or_default(),
        paused: seen.paused,
        open_tasks: seen.open_tasks,
        busy_with: active.as_ref().map(|a| a.job.as_str()).filter(|j| *j != seen.job.id),
    };
    entry.from = Some(&seen.job.from);
    entry.from_role = seen.job.from_role.as_deref();
    let name = match decide::decide(&facts, Ok(policy)) {
        Decision::Allow(name) => name,
        Decision::Deny(d) => return deny(&entry, &d.to_string()),
    };
    let reason = format!(
        "task from {} [main], hops {}, profile {name} is enabled",
        seen.job.from, seen.job.hops
    );
    entry.decision = "allow";
    entry.profile = Some(name.as_str());
    entry.reason = &reason;
    if let Err(e) = files.audit(&entry) {
        return deny(&entry, &format!("nothing is allowed without an audit line: {e}"));
    }
    println!("allow profile={name} reason={reason}");
    if !accept {
        return Ok(());
    }
    let accepted = c.accept(id)?;
    let active = Active {
        job: accepted.id,
        profile: name.as_str().to_owned(),
        from: seen.job.from.clone(),
        accepted_at: now,
        expires_at: now + policy.max_runtime_secs,
    };
    journal::write_active(&active_path, &active).map_err(|e| {
        RackError::Local(format!(
            "{} is accepted but its profile could not be activated: {e}",
            active.job
        ))
    })?;
    println!(
        "accepted {}: profile {name} is active for {} s, until rack policy release or expiry",
        active.job, policy.max_runtime_secs
    );
    Ok(())
}

pub fn release() -> Result<()> {
    let files = Files::locate()?;
    let path = journal::active_path(&files.dir);
    // Expired or damaged, the file goes all the same: releasing never widens what is allowed.
    let active = journal::read_active(&path, 0).ok().flatten();
    journal::clear_active(&path).map_err(RackError::Local)?;
    let Some(a) = active else {
        println!("no active profile");
        return Ok(());
    };
    let entry = Entry {
        source: "release",
        decision: "release",
        job: Some(&a.job),
        profile: Some(&a.profile),
        from: Some(&a.from),
        from_role: None,
        tool: None,
        reason: "released by rack policy release",
    };
    if let Err(e) = files.audit(&entry) {
        eprintln!("rack: {e}");
    }
    println!("released profile {} of {}", a.profile, a.job);
    Ok(())
}

pub fn hook() -> Result<()> {
    let mut stdin = String::new();
    // An unreadable stdin is judged like any unreadable input: denied, below.
    let _ = std::io::stdin().take(4 << 20).read_to_string(&mut stdin);
    let located = Files::locate();
    let exe = std::env::current_exe().ok();
    let exe = exe.as_deref().and_then(Path::to_str);
    let (verdict, tool, active, files) = match &located {
        Ok(f) => {
            let loaded = f.load();
            let active = journal::read_active(&journal::active_path(&f.dir), journal::now());
            let ctx = Context {
                policy: loaded.as_ref(),
                active: active.as_ref().map(Option::as_ref).map_err(String::as_str),
                rack_exe: exe,
            };
            let judged = hook::evaluate(&stdin, &ctx);
            (judged.verdict, judged.tool, active.ok().flatten(), Some(f))
        }
        Err(e) => (
            Verdict::Deny(format!("cannot locate the policy: {e}")),
            None,
            None,
            None,
        ),
    };
    let verdict = match (files, &verdict) {
        (_, Verdict::Abstain) | (None, _) => verdict,
        (Some(f), _) => {
            let entry = Entry {
                source: "hook",
                decision: verdict.name(),
                job: active.as_ref().map(|a| a.job.as_str()),
                profile: active.as_ref().map(|a| a.profile.as_str()),
                from: active.as_ref().map(|a| a.from.as_str()),
                from_role: None,
                tool: tool.as_deref(),
                reason: verdict.reason(),
            };
            match f.audit(&entry) {
                Ok(()) => verdict,
                Err(e) => Verdict::Deny(format!("nothing is allowed without an audit line: {e}")),
            }
        }
    };
    if let Some(json) = verdict.render() {
        println!("{json}");
    }
    Ok(())
}

pub fn settings() -> Result<()> {
    let files = Files::locate()?;
    let p = require(&files)?;
    let exe = std::env::current_exe().map_err(|e| local("find the rack binary", e))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| RackError::Local("the path of the rack binary is not UTF-8".to_owned()))?;
    let json = serde_json::to_string_pretty(&hook::settings(&p, exe)).map_err(|e| local("print settings", e))?;
    println!("{json}");
    Ok(())
}
