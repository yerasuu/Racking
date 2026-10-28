//! Pairing routes (#46): an admin makes an invite and gets a one-time code, the
//! other agent redeems it with `POST /join` and gets a token that does nothing
//! until the admin, having compared the fingerprint on both screens, confirms.
//! Also the links between agents that follow from it.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::admin::{admin, audited, record};
use super::{App, Call, Reply, header_value, ok};
use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::invite::{self, InviteCode, MAX_TTL_SECS};
use crate::pairing::{self, Roster};
use crate::policy::Role;
use crate::registry::{self, Caller, Source};
use crate::store::{Decision, Invite, InviteState, Redeem, bad_code, limits};
use crate::types::{AgentName, Label};

/// How often a long wait on an invite looks at the store again. The state can
/// change in another process (`racking-server invite`), so a condition variable
/// in this one would miss it.
const POLL: Duration = Duration::from_millis(250);

impl App {
    fn roster(&self) -> Roster {
        Roster {
            names: self.registry.names(),
            main: self.registry.main_agent(),
            roles_enforced: self.send_rules.enforced(),
        }
    }

    /// Denies the joiners whose inviter let the confirmation time run out.
    fn deny_late(&self, now: i64) {
        match self.store.deny_unconfirmed(now) {
            Ok(names) => {
                for name in names {
                    record(self, "server", "invite.deny", Some(name.as_str()), "timeout");
                    tracing::info!(target: "racking::pairing", agent = %name, "joiner denied: no confirmation in time");
                    self.accounts().forget(&name);
                }
            }
            Err(e) => {
                tracing::warn!(target: "racking::pairing", error = %e.message, "could not deny late joiners");
            }
        }
    }

    /// Brings a confirmed joiner into force in memory: its link, and its standing.
    fn adopt(&self, invite: &Invite) {
        if let Some(inviter) = &invite.inviter {
            self.links.insert(inviter, &invite.name);
        }
        self.registry.confirm(&invite.name);
    }

    /// A pending caller, checked against the store, which another process may have
    /// changed: confirmed callers go on, denied ones lose their token.
    pub(super) fn settle_pending(&self, caller: Caller) -> ApiResult<Caller> {
        let now = self.clock.now();
        self.deny_late(now);
        match self.store.invite_of(&caller.name, now)? {
            Some(invite) if invite.state == InviteState::Confirmed => {
                self.adopt(&invite);
                Ok(Caller {
                    pending: false,
                    ..caller
                })
            }
            Some(invite) if invite.state == InviteState::Redeemed => Err(ApiError::new(
                Code::NotConfirmed,
                format!("agent {} is waiting for its inviter's confirmation", caller.name),
                format!(
                    "The person who ran `rack link` must compare the words `{}` on both screens and confirm. Until \
                     then only GET /join/status works; `rack join` waits for it.",
                    invite.fingerprint().unwrap_or_default()
                ),
            )),
            Some(_) | None => {
                self.accounts().forget(&caller.name);
                Err(ApiError::new(Code::BadToken, "unknown token", super::AUTH_HINT))
            }
        }
    }

    /// Counts a wrong code: against the peer, and against every open invite.
    fn bad_code(&self, peer: IpAddr, now: i64) -> ApiError {
        let _ = self.join_failures.admit(&peer);
        let peer_text = peer.to_string();
        record(self, &peer_text, "join", None, Code::InviteRefused.as_str());
        tracing::warn!(target: "racking::pairing", peer = %peer, "wrong pairing code");
        match self.store.count_bad_code(now) {
            Ok(voided) => {
                for id in voided {
                    record(
                        self,
                        &peer_text,
                        "invite.void",
                        Some(&id.to_string()),
                        "too_many_failures",
                    );
                    tracing::warn!(target: "racking::pairing", invite = id, "invite voided after too many wrong codes");
                }
            }
            Err(e) => tracing::warn!(target: "racking::pairing", error = %e.message, "could not count a wrong code"),
        }
        bad_code()
    }
}

/// `timeout` of a long wait: 0 when absent, cut to the longest wait.
fn wait_secs(call: &Call<'_>) -> ApiResult<u64> {
    call.query_raw("timeout").map_or(Ok(0), |text| {
        text.parse::<u64>()
            .map(|t| t.min(limits::WAIT_MAX_SECS))
            .map_err(|_| ApiError::bad_request("timeout: whole seconds, 0 to 55"))
    })
}

fn invite_param(call: &Call<'_>) -> ApiResult<i64> {
    let raw = call.params.first().copied().unwrap_or_default();
    raw.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| ApiError::bad_request("invite id: a positive integer, as GET /admin/invites lists it"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewInviteBody {
    #[serde(default)]
    name: Option<String>,
    role: String,
    #[serde(default)]
    ttl_secs: Option<i64>,
}

pub(super) fn create_invite(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let input: NewInviteBody = call.body()?;
    let me = admin(app, call, "invite.create", input.name.as_deref())?;
    let name = input
        .name
        .as_deref()
        .map(str::parse::<AgentName>)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("name: {e}")))?;
    let role: Role = input
        .role
        .parse()
        .map_err(|e: String| ApiError::bad_request(format!("role: {e}")))?;
    let ttl = input.ttl_secs.unwrap_or(invite::DEFAULT_TTL_SECS);
    if !(1..=MAX_TTL_SECS).contains(&ttl) {
        return Err(ApiError::bad_request(format!("ttl_secs: 1 to {MAX_TTL_SECS}")));
    }
    let target = name.as_ref().map_or("-", AgentName::as_str).to_owned();
    let now = app.clock.now();
    let made = audited(app, me, "invite.create", &target, || {
        pairing::make_invite(&app.store, &app.roster(), Some(&me.name), name, role, ttl, now)
    })?;
    tracing::info!(target: "racking::pairing", actor = %me.name, invite = made.id, agent = %made.name, role = %role, "invite made");
    Ok(Reply::Json {
        status: 201,
        body: json!({
            "id": made.id,
            "code": made.code.to_string(),
            "name": made.name.as_str(),
            "role": made.role.as_str(),
            "expires_at": clock::rfc3339(made.expires_at),
            "note": "The code is shown once and stored only as a salted hash. Give it to the other agent's owner \
                     by a channel you trust, then wait for it with GET /admin/invites/{id}?timeout=25 and compare \
                     the fingerprint on both screens before confirming.",
        }),
    })
}

pub(super) fn list_invites(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    admin(app, call, "invite.list", None)?;
    let now = app.clock.now();
    app.deny_late(now);
    let invites = app.store.pending_invites(now)?;
    ok(json!({"invites": invites.iter().map(Invite::to_json).collect::<Vec<_>>()}))
}

pub(super) fn get_invite(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id = invite_param(call)?;
    admin(app, call, "invite.read", Some(&id.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(wait_secs(call)?);
    loop {
        let now = app.clock.now();
        app.deny_late(now);
        let invite = app.store.invite(id, now)?.ok_or_else(|| {
            ApiError::new(
                Code::NotFound,
                format!("no invite {id}"),
                "GET /admin/invites lists the pending ones.",
            )
        })?;
        if invite.state != InviteState::Open || Instant::now() >= deadline {
            return ok(invite.to_json());
        }
        std::thread::sleep(POLL);
    }
}

pub(super) fn cancel_invite(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id = invite_param(call)?;
    let target = id.to_string();
    let me = admin(app, call, "invite.cancel", Some(&target))?;
    let invite = audited(app, me, "invite.cancel", &target, || {
        app.store.void_invite(id, app.clock.now())
    })?;
    ok(invite.to_json())
}

pub(super) fn confirm_invite(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    decide(app, call, Decision::Confirm)
}

pub(super) fn deny_invite(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    decide(app, call, Decision::Deny)
}

fn decide(app: &App, call: &mut Call<'_>, decision: Decision) -> ApiResult<Reply> {
    let id = invite_param(call)?;
    let target = id.to_string();
    let action = match decision {
        Decision::Confirm => "invite.confirm",
        Decision::Deny => "invite.deny",
    };
    let me = admin(app, call, action, Some(&target))?;
    let now = app.clock.now();
    app.deny_late(now);
    let result = audited(app, me, action, &target, || app.store.decide(id, decision, now));
    match &result {
        Ok(invite) if invite.state == InviteState::Confirmed => app.adopt(invite),
        Ok(invite) => app.accounts().forget(&invite.name),
        // Too late: the store denied it in the same transaction.
        Err(_) => {
            if let Ok(Some(invite)) = app.store.invite(id, now)
                && invite.state == InviteState::Denied
            {
                app.accounts().forget(&invite.name);
            }
        }
    }
    let invite = result?;
    tracing::info!(target: "racking::pairing", actor = %me.name, invite = id, agent = %invite.name, action, "invite decided");
    ok(invite.to_json())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinBody {
    code: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    rack_version: Option<String>,
}

/// The single validation point of a join: the body's fields, then the code.
pub(super) fn join(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let peer = call.client.addr;
    if let Some(wait) = app.join_failures.blocked(&peer) {
        return Err(ApiError::new(
            Code::RateLimited,
            "too many wrong pairing codes from your address",
            format!("Retry-After: {wait}. Ask the person who ran `rack link` for the code again."),
        )
        .with_retry_after(wait));
    }
    let input: JoinBody = call.body()?;
    let wanted = input
        .name
        .as_deref()
        .map(str::parse::<AgentName>)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("name: {e}")))?;
    let label = |field: &str, value: Option<&str>| -> ApiResult<Option<String>> {
        value
            .map(|v| v.parse::<Label>().map(Label::into_inner))
            .transpose()
            .map_err(|e| ApiError::bad_request(format!("{field}: {e}")))
    };
    let os = label("os", input.os.as_deref())?;
    let rack_version = label("rack_version", input.rack_version.as_deref())?;
    let now = app.clock.now();
    app.deny_late(now);

    // Every live invite is hashed and compared without stopping at a match, so the
    // time taken depends on how many invites are open, not on which code was sent.
    let presented = input.code.parse::<InviteCode>().ok();
    let mut matched = None;
    for live in app.store.live_codes(now)? {
        if invite::matches(presented.as_ref(), &live.salt, &live.hash) {
            matched = Some(live.id);
        }
    }
    let Some(id) = matched else {
        return Err(app.bad_code(peer, now));
    };
    let invite = app.store.invite(id, now)?.ok_or_else(bad_code)?;
    let name = match wanted {
        Some(n) if n != invite.name && invite.name_chosen => {
            return Err(ApiError::new(
                Code::Conflict,
                format!("this invite is for an agent named {}", invite.name),
                format!(
                    "Leave the name out, or pass `{}`; the code is still valid.",
                    invite.name
                ),
            ));
        }
        Some(n) if n != invite.name && app.store.name_reserved(&n, now)? => {
            return Err(ApiError::new(
                Code::Conflict,
                format!("the name {n} is taken by another invite"),
                "Pick another name, or leave it out; the code is still valid.",
            ));
        }
        Some(n) => n,
        None => invite.name.clone(),
    };
    let peer_text = peer.to_string();
    let issued = app.accounts().create_pending(&name, invite.role, |digest, _| {
        app.store.redeem(
            &Redeem {
                invite: id,
                name: &name,
                role: invite.role,
                token_digest: digest,
                peer: &peer_text,
                os: os.as_deref(),
                rack_version: rack_version.as_deref(),
            },
            now,
        )
    });
    let issued = match issued {
        Ok(issued) => issued,
        Err(e) => {
            record(app, name.as_str(), "join", Some(&id.to_string()), e.code.as_str());
            return Err(e);
        }
    };
    record(app, name.as_str(), "join", Some(&id.to_string()), "ok");
    tracing::info!(target: "racking::pairing", agent = %name, invite = id, peer = %peer, "code redeemed; waiting for confirmation");
    let fingerprint = invite::fingerprint(id, &name, &registry::digest(issued.token.as_str()));
    Ok(Reply::Json {
        status: 201,
        body: json!({
            "name": issued.name.as_str(),
            "role": issued.role.as_str(),
            "token": issued.token.as_str(),
            "invite": id,
            "inviter": invite.inviter.as_ref().map(AgentName::as_str),
            "fingerprint": fingerprint,
            "state": "pending",
            "confirm_by": clock::rfc3339(now + invite::CONFIRM_SECS),
            "note": "This token is shown once and stored only as a hash. It works only once your inviter has \
                     compared the fingerprint on both screens and confirmed; wait with GET /join/status.",
        }),
    })
}

/// Answers a joiner by its token, whatever became of its agent: the token may
/// already be dead after a denial, and the joiner still needs to hear it.
pub(super) fn join_status(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let header = header_value(call.request, "Authorization")
        .ok_or_else(|| ApiError::new(Code::NoToken, "missing Authorization header", super::AUTH_HINT))?;
    let token = header.strip_prefix("Bearer ").map(str::trim).unwrap_or_default();
    let digest = registry::digest(token);
    let deadline = Instant::now() + Duration::from_secs(wait_secs(call)?);
    loop {
        let now = app.clock.now();
        app.deny_late(now);
        let invite = app
            .store
            .invite_by_token(&digest, now)?
            .ok_or_else(|| ApiError::new(Code::BadToken, "unknown token", super::AUTH_HINT))?;
        let state = match invite.state {
            InviteState::Redeemed if Instant::now() < deadline => {
                std::thread::sleep(POLL);
                continue;
            }
            InviteState::Redeemed => "pending",
            InviteState::Confirmed => {
                app.adopt(&invite);
                "confirmed"
            }
            InviteState::Denied | InviteState::Open | InviteState::Expired | InviteState::Voided => {
                app.accounts().forget(&invite.name);
                "denied"
            }
        };
        return ok(json!({
            "state": state,
            "name": invite.name.as_str(),
            "role": invite.role.as_str(),
            "inviter": invite.inviter.as_ref().map(AgentName::as_str),
            "fingerprint": invite.fingerprint(),
            "confirm_by": invite.confirm_by().map(clock::rfc3339),
        }));
    }
}

pub(super) fn my_links(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let me = call.caller()?;
    let rows = app.store.links_of(&me.name)?;
    let members = app.registry.members();
    let role_of = |n: &AgentName| members.iter().find(|m| &m.name == n).map(|m| m.role.as_str());
    let mut links: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "agent": r.with.as_str(),
                "role": role_of(&r.with),
                "state": r.state,
                "via": r.origin,
                "created_at": clock::rfc3339(r.created_at),
                "confirmed_at": r.confirmed_at.map(clock::rfc3339),
            })
        })
        .collect();
    for m in &members {
        let implicit = m.name != me.name
            && !m.pending
            && (me.source == Source::File || m.source == Source::File)
            && !rows.iter().any(|r| r.with == m.name);
        if implicit {
            links.push(json!({
                "agent": m.name.as_str(),
                "role": m.role.as_str(),
                "state": "confirmed",
                "via": "config",
                "created_at": null,
                "confirmed_at": null,
            }));
        }
    }
    ok(json!({
        "agent": me.name.as_str(),
        "source": me.source.as_str(),
        "links": links,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLinkBody {
    a: String,
    b: String,
}

pub(super) fn create_link(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let input: NewLinkBody = call.body()?;
    let target = format!("{},{}", super::truncate(&input.a, 40), super::truncate(&input.b, 40));
    let me = admin(app, call, "link.add", Some(&target))?;
    let parse = |field: &str, text: &str| {
        text.parse::<AgentName>()
            .map_err(|e| ApiError::bad_request(format!("{field}: {e}")))
    };
    let (a, b) = (parse("a", &input.a)?, parse("b", &input.b)?);
    audited(app, me, "link.add", &target, || {
        if a == b {
            return Err(ApiError::bad_request("a and b must be two agents"));
        }
        for n in [&a, &b] {
            let member = app.registry.member(n).ok_or_else(|| {
                ApiError::new(
                    Code::NotFound,
                    format!("no agent named {n}"),
                    "GET /admin/agents lists the agents.",
                )
            })?;
            if member.pending {
                return Err(ApiError::new(
                    Code::Conflict,
                    format!("{n} is waiting for its confirmation"),
                    "Confirm its invite first.",
                ));
            }
            if member.role == Role::Main {
                return Err(ApiError::new(
                    Code::Conflict,
                    format!("{n} is main"),
                    "The main agent is linked only through invites (rack link); this route links two workers.",
                ));
            }
        }
        app.store.add_link(&a, &b, app.clock.now())
    })?;
    app.links.insert(&a, &b);
    tracing::info!(target: "racking::pairing", actor = %me.name, a = %a, b = %b, "workers linked");
    Ok(Reply::Json {
        status: 201,
        body: json!({"a": a.as_str(), "b": b.as_str(), "state": "confirmed", "via": "admin"}),
    })
}

/// Removes a link. An agent that joined through an invite of the other agent
/// exists for that link, so it loses its token too; its jobs are kept. Agents of
/// the config file are never removed here.
pub(super) fn delete_link(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let a: AgentName = call.param(0)?;
    let b: AgentName = call.param(1)?;
    let target = format!("{a},{b}");
    let me = admin(app, call, "link.remove", Some(&target))?;
    let now = app.clock.now();
    let revoked = audited(app, me, "link.remove", &target, || {
        if app.store.delete_link(&a, &b)?.is_none() {
            return Err(ApiError::new(
                Code::NotFound,
                format!("no stored link between {a} and {b}"),
                "GET /agent-links (as either agent) lists the links; agents of the config file are linked with \
                 everyone and cannot be unlinked.",
            ));
        }
        app.links.remove(&a, &b);
        let mut revoked = Vec::with_capacity(2);
        for (joiner, other) in [(&a, &b), (&b, &a)] {
            let api = app.registry.member(joiner).is_some_and(|m| m.source == Source::Api);
            let invited_by_other = app
                .store
                .invite_of(joiner, now)?
                .is_some_and(|i| i.inviter.as_ref() == Some(other));
            if api && invited_by_other {
                app.accounts().revoke(joiner)?;
                revoked.push(joiner.as_str().to_owned());
            }
        }
        Ok(revoked)
    })?;
    tracing::info!(target: "racking::pairing", actor = %me.name, a = %a, b = %b, ?revoked, "link removed");
    ok(json!({"a": a.as_str(), "b": b.as_str(), "removed": true, "revoked": revoked}))
}
