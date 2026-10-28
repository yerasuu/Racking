//! Broadcasts (#28): one notice, message or report to several agents. Each recipient
//! gets a job of its own, so inbox, delivery, answers and the outbox work as for one;
//! the jobs share a `group` id. The send rules are checked per recipient, and a name
//! the sender may not reach is skipped the same way whether it exists or not, so a
//! broadcast cannot be used to learn which agents exist.

use serde_json::{Value, json};

use super::{App, CreateJobBody, Reply, admit, check_paused, reach_hint, validate_fields};
use crate::error::{ApiError, ApiResult, Code};
use crate::registry::Caller;
use crate::store::{Kind, limits};
use crate::types::AgentName;

/// Who a `POST /jobs` goes to, read from its `to`.
pub(super) enum Target {
    One(AgentName),
    Many(Recipients),
}

pub(super) enum Recipients {
    /// Distinct names, in the order given, the sender not among them.
    Named(Vec<AgentName>),
    /// `"*"`: every other agent the sender may start a job for.
    Everyone,
}

/// The single validation point of `to`: an agent name, a list of names, or `"*"`.
pub(super) fn target(me: &Caller, to: &Value) -> ApiResult<Target> {
    let name = |text: &str| -> ApiResult<AgentName> {
        let name: AgentName = text
            .parse()
            .map_err(|e: String| ApiError::bad_request(format!("to: {e}")))?;
        if name == me.name {
            return Err(ApiError::bad_request("to: you cannot send a job to yourself"));
        }
        Ok(name)
    };
    match to {
        Value::String(all) if all == "*" => Ok(Target::Many(Recipients::Everyone)),
        Value::String(one) => name(one).map(Target::One),
        Value::Array(items) => {
            if items.is_empty() || items.len() > limits::RECIPIENTS {
                return Err(ApiError::bad_request(format!(
                    "to: a list of 1 to {} agent names",
                    limits::RECIPIENTS
                )));
            }
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request("to: every item of the list is an agent name"))?;
                let parsed = name(text)?;
                if !names.contains(&parsed) {
                    names.push(parsed);
                }
            }
            Ok(Target::Many(Recipients::Named(names)))
        }
        _ => Err(ApiError::bad_request(
            "to: an agent name, a list of agent names, or \"*\" for every agent you may send to",
        )),
    }
}

/// `POST /jobs` with several recipients: one job each, in one transaction and one group.
pub(super) fn send(app: &App, me: &Caller, input: CreateJobBody, recipients: Recipients) -> ApiResult<Reply> {
    let kind = input
        .kind
        .as_deref()
        .map(str::parse::<Kind>)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("kind: {e}")))?
        .unwrap_or(Kind::Task);
    if !kind.is_delivery() {
        return Err(ApiError::new(
            Code::BadRequest,
            format!("to: a {} goes to exactly one agent", kind.as_str()),
            "Only a notice, message or report may go to a list or to \"*\". Send one task per agent, each with \
             its own title and body, so each recipient accepts and finishes its own.",
        ));
    }
    if input.reply_to.is_some() {
        return Err(ApiError::new(
            Code::BadRequest,
            "reply_to: a reply goes to the other party of the job it answers, never to a list",
            "Answer with `to` set to that one agent.",
        ));
    }
    let reach = Reach::of(app, me, recipients);
    if reach.allowed.len() > limits::RECIPIENTS {
        return Err(ApiError::new(
            Code::BadRequest,
            format!(
                "to: \"*\" reaches {} agents, over {}",
                reach.allowed.len(),
                limits::RECIPIENTS
            ),
            format!(
                "Name at most {} of your peers (GET /whoami) in a list.",
                limits::RECIPIENTS
            ),
        ));
    }
    let Some(first) = reach.allowed.first() else {
        return Err(ApiError::new(
            Code::NotAllowed,
            format!("{} ({}) may send to none of these agents", me.name, me.role),
            reach_hint(app, me),
        ));
    };
    let new = validate_fields(app, me, first.clone(), input)?;
    admit(app, me, reach.allowed.len())?;
    check_paused(app, me)?;
    let jobs = app.store.create_group(&me.name, &reach.allowed, &new)?;
    let group = jobs.first().and_then(|j| j.group).map(|g| format!("g{g}"));
    tracing::info!(
        target: "racking::broadcast",
        agent = %me.name,
        group = group.as_deref().unwrap_or("-"),
        jobs = jobs.len(),
        skipped = reach.skipped_count,
        "broadcast"
    );
    Ok(Reply::Json {
        status: 201,
        body: json!({
            "group": group,
            "jobs": jobs.iter().map(|j| json!({"to": j.to, "id": j.id.to_string(), "state": j.state.as_str()}))
                .collect::<Vec<_>>(),
            "skipped": reach.skipped.iter().map(|to| json!({"to": to.as_str(), "reason": "not_allowed"}))
                .collect::<Vec<_>>(),
            "skipped_count": reach.skipped_count,
        }),
    })
}

/// Which recipients the sender may start a job for, by `SendRules::may_send`, which also
/// refuses a pending agent.
struct Reach {
    allowed: Vec<AgentName>,
    /// Names the sender gave and may not reach; empty for `"*"`, which names nobody.
    skipped: Vec<AgentName>,
    skipped_count: usize,
}

impl Reach {
    fn of(app: &App, me: &Caller, recipients: Recipients) -> Self {
        let party = me.party();
        match recipients {
            Recipients::Named(names) => {
                let (allowed, skipped): (Vec<_>, Vec<_>) = names.into_iter().partition(|name| {
                    app.registry
                        .member(name)
                        .is_some_and(|m| app.send_rules.may_send(&party, &m.party(), &app.links))
                });
                let skipped_count = skipped.len();
                Self {
                    allowed,
                    skipped,
                    skipped_count,
                }
            }
            Recipients::Everyone => {
                let others: Vec<_> = app
                    .registry
                    .members()
                    .into_iter()
                    .filter(|m| m.name != me.name)
                    .collect();
                let allowed: Vec<_> = others
                    .iter()
                    .filter(|m| app.send_rules.may_send(&party, &m.party(), &app.links))
                    .map(|m| m.name.clone())
                    .collect();
                Self {
                    skipped_count: others.len() - allowed.len(),
                    allowed,
                    skipped: Vec::new(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Role;
    use crate::registry::Source;

    fn me() -> Caller {
        Caller {
            name: "boss".parse().expect("name"),
            role: Role::Main,
            admin: false,
            source: Source::File,
            pending: false,
        }
    }

    fn named(to: Value) -> Result<Vec<String>, String> {
        match target(&me(), &to) {
            Ok(Target::Many(Recipients::Named(names))) => Ok(names.iter().map(|n| n.as_str().to_owned()).collect()),
            Ok(Target::One(n)) => Err(format!("one: {n}")),
            Ok(Target::Many(Recipients::Everyone)) => Err("everyone".to_owned()),
            Err(e) => Err(e.message),
        }
    }

    #[test]
    fn to_is_a_name_a_list_or_everyone() {
        assert!(matches!(target(&me(), &json!("w1")), Ok(Target::One(_))));
        assert!(matches!(
            target(&me(), &json!("*")),
            Ok(Target::Many(Recipients::Everyone))
        ));
        assert_eq!(
            named(json!(["w2", "w1", "w2"])),
            Ok(vec!["w2".to_owned(), "w1".to_owned()])
        );
        assert_eq!(
            named(json!(["w1"])),
            Ok(vec!["w1".to_owned()]),
            "a list of one is still a list"
        );
        let many: Vec<String> = (0..=limits::RECIPIENTS).map(|i| format!("a{i}")).collect();
        for bad in [
            json!([]),
            json!(many),
            json!(["w1", 3]),
            json!(["*"]),
            json!(7),
            json!(null),
        ] {
            let err = named(bad.clone()).expect_err("refused");
            assert!(err.starts_with("to:"), "{bad}: {err}");
        }
        assert!(named(json!(["w1", "boss"])).is_err_and(|e| e.contains("yourself")));
        assert!(named(json!("boss")).is_err_and(|e| e.contains("yourself")));
        assert!(named(json!(["W1"])).is_err(), "names are checked like single ones");
    }
}
