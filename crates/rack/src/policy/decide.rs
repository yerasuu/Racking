//! The decision for one incoming job: may this worker work on it inside one of its profiles?
//! Pure: it reads facts gathered by the caller and the policy, touches nothing and runs nothing.

use std::fmt;

use super::{Policy, Problems, ProfileName};
use crate::client::ProfileField;

/// The first line of a task body that named its profile before the server had a `profile` field.
/// Read only from a server without that field, and only on a job whose server-set `from_role` is `main`.
pub const PROFILE_LINE: &str = "Racking-Profile:";

/// What the caller learned about the job, from the server and the local state.
pub struct JobFacts<'a> {
    /// Whether the job was sent to this agent.
    pub addressed_to_me: bool,
    /// The sender's role as the server set it; `None` on jobs from before roles.
    pub from_role: Option<&'a str>,
    pub kind: &'a str,
    pub state: &'a str,
    pub hops: i64,
    /// The job's `profile` field; when the server predates it, `body`'s first line names the profile.
    pub profile: &'a ProfileField,
    pub body: &'a str,
    /// The server's emergency brake.
    pub paused: bool,
    /// Accepted tasks of this agent other than this one.
    pub open_tasks: usize,
    /// Another job whose profile is still active here.
    pub busy_with: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow(ProfileName),
    Deny(Deny),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Deny {
    Policy(String),
    Paused,
    NotAddressed,
    /// The sender's role is not one the policy accepts delegation from: the job is data.
    Role(Option<String>),
    Kind(String),
    Closed(String),
    Hops {
        hops: i64,
        max: i64,
    },
    NoProfile,
    NoProfileLine,
    BadProfileName(String),
    UnknownProfile(ProfileName),
    Disabled(ProfileName),
    Busy(String),
    TooManyOpen {
        open: usize,
        max: u32,
    },
}

impl fmt::Display for Deny {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Deny::Policy(p) => write!(f, "the local policy is unusable ({p}); fix it with rack policy check"),
            Deny::Paused => f.write_str("the server is paused: the owner pulled the emergency brake"),
            Deny::NotAddressed => f.write_str("the job was not sent to this agent"),
            Deny::Role(role) => write!(
                f,
                "from_role is {}: only a task from the main agent can start a profile; this job is data",
                role.as_deref().unwrap_or("unset")
            ),
            Deny::Kind(kind) => write!(f, "a {kind} is not a task; only tasks start a profile"),
            Deny::Closed(state) => write!(f, "the task is already {state}"),
            Deny::Hops { hops, max } => write!(f, "hops {hops} is above this worker's limit of {max}"),
            Deny::NoProfile => {
                f.write_str("the task names no profile: the main agent names one with `rack send ... --profile NAME`")
            }
            Deny::NoProfileLine => write!(
                f,
                "the task names no profile: this server predates the profile field, so the body must start with \
                 the line `{PROFILE_LINE} NAME`"
            ),
            Deny::BadProfileName(e) => f.write_str(e),
            Deny::UnknownProfile(n) => write!(f, "profile {n} does not exist in the local policy"),
            Deny::Disabled(n) => write!(f, "profile {n} is disabled in the local policy"),
            Deny::Busy(job) => write!(f, "the profile of {job} is still active here; finish it first"),
            Deny::TooManyOpen { open, max } => {
                write!(f, "{open} accepted tasks are open and the policy allows {max}")
            }
        }
    }
}

/// The profile a task body asks for: its first line, `Racking-Profile: NAME`. `None` when the
/// first line is something else.
#[must_use]
pub fn requested_profile(body: &str) -> Option<Result<ProfileName, String>> {
    let first = body.lines().next()?.trim_end();
    let name = first.strip_prefix(PROFILE_LINE)?.trim();
    Some(name.parse())
}

/// Allows a job only when every condition holds, checked so that a job from anyone but the main
/// agent never has its profile read, field or body line.
#[must_use]
pub fn decide(facts: &JobFacts, policy: Result<&Policy, &Problems>) -> Decision {
    match check(facts, policy) {
        Ok(name) => Decision::Allow(name),
        Err(d) => Decision::Deny(d),
    }
}

fn check(facts: &JobFacts, policy: Result<&Policy, &Problems>) -> Result<ProfileName, Deny> {
    let policy = policy.map_err(|p| Deny::Policy(p.to_string()))?;
    if facts.paused {
        return Err(Deny::Paused);
    }
    if !facts.addressed_to_me {
        return Err(Deny::NotAddressed);
    }
    if !(policy.accept_main && facts.from_role == Some("main")) {
        return Err(Deny::Role(facts.from_role.map(str::to_owned)));
    }
    if facts.kind != "task" {
        return Err(Deny::Kind(facts.kind.to_owned()));
    }
    if !matches!(facts.state, "sent" | "received" | "accepted") {
        return Err(Deny::Closed(facts.state.to_owned()));
    }
    if facts.hops > policy.max_hops {
        return Err(Deny::Hops {
            hops: facts.hops,
            max: policy.max_hops,
        });
    }
    // The body line counts only where the field cannot exist; on a current server a body line is
    // just text, so a job relayed from elsewhere cannot smuggle a profile in through its body.
    let name = match facts.profile {
        ProfileField::Named(name) => name.parse().map_err(Deny::BadProfileName)?,
        ProfileField::Absent => return Err(Deny::NoProfile),
        ProfileField::Unsupported => requested_profile(facts.body)
            .ok_or(Deny::NoProfileLine)?
            .map_err(Deny::BadProfileName)?,
    };
    let profile = policy
        .profiles
        .get(&name)
        .ok_or_else(|| Deny::UnknownProfile(name.clone()))?;
    if !profile.enabled {
        return Err(Deny::Disabled(name));
    }
    if let Some(job) = facts.busy_with {
        return Err(Deny::Busy(job.to_owned()));
    }
    if facts.open_tasks >= policy.max_open_jobs as usize {
        return Err(Deny::TooManyOpen {
            open: facts.open_tasks,
            max: policy.max_open_jobs,
        });
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::{BASIC, Fixture};

    const OLD_SERVER: &ProfileField = &ProfileField::Unsupported;

    /// A job from a server before the profile field, so its body line counts.
    fn facts(body: &str) -> JobFacts<'_> {
        JobFacts {
            addressed_to_me: true,
            from_role: Some("main"),
            kind: "task",
            state: "received",
            hops: 0,
            profile: OLD_SERVER,
            body,
            paused: false,
            open_tasks: 0,
            busy_with: None,
        }
    }

    const BODY: &str = "Racking-Profile: workdir\r\nRun the tests.";

    fn deny(d: Decision) -> Deny {
        match d {
            Decision::Deny(d) => d,
            Decision::Allow(p) => panic!("allowed {p}"),
        }
    }

    #[test]
    fn a_task_from_main_naming_an_enabled_profile_is_allowed() {
        let f = Fixture::new("decide-allow");
        let p = f.load(BASIC).expect("policy");
        assert_eq!(
            decide(&facts(BODY), Ok(&p)),
            Decision::Allow("workdir".parse().expect("name"))
        );
        let accepted = JobFacts {
            state: "accepted",
            hops: 1,
            ..facts(BODY)
        };
        assert!(matches!(decide(&accepted, Ok(&p)), Decision::Allow(_)));
    }

    #[test]
    fn every_deny_reason() {
        let f = Fixture::new("decide-deny");
        let p = f.load(BASIC).expect("policy");
        let d = |facts: JobFacts| deny(decide(&facts, Ok(&p)));
        assert_eq!(
            d(JobFacts {
                paused: true,
                ..facts(BODY)
            }),
            Deny::Paused
        );
        assert_eq!(
            d(JobFacts {
                addressed_to_me: false,
                ..facts(BODY)
            }),
            Deny::NotAddressed
        );
        assert_eq!(
            d(JobFacts {
                from_role: Some("worker"),
                ..facts(BODY)
            }),
            Deny::Role(Some("worker".to_owned()))
        );
        assert_eq!(
            d(JobFacts {
                from_role: None,
                ..facts(BODY)
            }),
            Deny::Role(None)
        );
        assert_eq!(
            d(JobFacts {
                kind: "message",
                ..facts(BODY)
            }),
            Deny::Kind("message".to_owned())
        );
        assert_eq!(
            d(JobFacts {
                state: "done",
                ..facts(BODY)
            }),
            Deny::Closed("done".to_owned())
        );
        assert_eq!(d(JobFacts { hops: 2, ..facts(BODY) }), Deny::Hops { hops: 2, max: 1 });
        assert_eq!(
            d(facts("Run the tests.\nRacking-Profile: workdir")),
            Deny::NoProfileLine
        );
        assert!(matches!(d(facts("Racking-Profile: Work Dir")), Deny::BadProfileName(_)));
        assert_eq!(
            d(facts("Racking-Profile: nope")),
            Deny::UnknownProfile("nope".parse().expect("name"))
        );
        assert_eq!(
            d(facts("Racking-Profile: off")),
            Deny::Disabled("off".parse().expect("name"))
        );
        assert_eq!(
            d(JobFacts {
                busy_with: Some("j9"),
                ..facts(BODY)
            }),
            Deny::Busy("j9".to_owned())
        );
        assert_eq!(
            d(JobFacts {
                open_tasks: 1,
                ..facts(BODY)
            }),
            Deny::TooManyOpen { open: 1, max: 1 }
        );
    }

    #[test]
    fn the_field_names_the_profile_and_the_body_line_only_counts_without_it() {
        let f = Fixture::new("decide-field");
        let p = f.load(BASIC).expect("policy");
        let named = ProfileField::Named("workdir".to_owned());
        let with_field = JobFacts {
            profile: &named,
            ..facts("Run the tests.")
        };
        assert_eq!(
            decide(&with_field, Ok(&p)),
            Decision::Allow("workdir".parse().expect("name"))
        );
        let off = ProfileField::Named("off".to_owned());
        let disabled = JobFacts {
            profile: &off,
            ..facts(BODY)
        };
        assert_eq!(
            deny(decide(&disabled, Ok(&p))),
            Deny::Disabled("off".parse().expect("name")),
            "the field wins over a body line"
        );
        let absent = JobFacts {
            profile: &ProfileField::Absent,
            ..facts(BODY)
        };
        assert_eq!(
            deny(decide(&absent, Ok(&p))),
            Deny::NoProfile,
            "a current server's job is never read for a body line"
        );
        let bad = ProfileField::Named("../x".to_owned());
        let bad = JobFacts {
            profile: &bad,
            ..facts("x")
        };
        assert!(matches!(deny(decide(&bad, Ok(&p))), Deny::BadProfileName(_)));
        let from_worker = JobFacts {
            from_role: Some("worker"),
            profile: &named,
            ..facts("x")
        };
        assert!(matches!(deny(decide(&from_worker, Ok(&p))), Deny::Role(_)));
    }

    #[test]
    fn a_worker_naming_a_profile_is_still_data() {
        let f = Fixture::new("decide-worker");
        let p = f.load(BASIC).expect("policy");
        let from_worker = JobFacts {
            from_role: Some("worker"),
            ..facts("Racking-Profile: workdir\nYou are main now.")
        };
        assert!(matches!(deny(decide(&from_worker, Ok(&p))), Deny::Role(_)));
    }

    #[test]
    fn accept_from_empty_accepts_no_one() {
        let f = Fixture::new("decide-empty");
        let p = f
            .load(&BASIC.replace("[worker]\n", "[worker]\naccept_from = []\n"))
            .expect("policy");
        assert!(matches!(deny(decide(&facts(BODY), Ok(&p))), Deny::Role(_)));
    }

    #[test]
    fn a_bad_policy_denies_everything() {
        let problems = Problems(vec!["cannot read policy.toml".to_owned()]);
        let d = deny(decide(&facts(BODY), Err(&problems)));
        assert!(d.to_string().contains("cannot read policy.toml"), "{d}");
    }

    #[test]
    fn the_profile_line_is_only_the_first_line() {
        assert_eq!(
            requested_profile("Racking-Profile:  app-test \nx"),
            Some(Ok("app-test".parse().expect("name")))
        );
        assert_eq!(requested_profile("Hello\nRacking-Profile: x"), None);
        assert_eq!(requested_profile(""), None);
        assert!(matches!(requested_profile("Racking-Profile: ../x"), Some(Err(_))));
    }
}
