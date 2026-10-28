//! The admin routes (#31): manage agents without editing the config file, read
//! the audit log, and pull or release the emergency brake. Every route checks
//! the caller's `admin` flag first; account changes themselves live in `accounts`.

use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{App, Call, Reply, ok};
use crate::clock;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy::Role;
use crate::registry::Caller;
use crate::store::{AuditEntry, limits};
use crate::types::{AgentName, truncate};

/// Records one audit row. A failure is logged and does not undo the action,
/// which has already happened by then.
pub(super) fn record(app: &App, actor: &str, action: &str, target: Option<&str>, result: &str) {
    let entry = AuditEntry {
        actor,
        action,
        target,
        result,
    };
    if let Err(e) = app.store.audit(&entry) {
        tracing::warn!(target: "racking::admin", action, error = %e.message, "could not write the audit log");
    }
}

/// The caller, if it is an admin; a refusal is audited, since probing the admin API is worth knowing about.
pub(super) fn admin<'c>(app: &App, call: &'c Call<'_>, action: &str, target: Option<&str>) -> ApiResult<&'c Caller> {
    let me = call.caller()?;
    if me.admin {
        return Ok(me);
    }
    record(app, me.name.as_str(), action, target, Code::NotAdmin.as_str());
    tracing::warn!(target: "racking::admin", agent = %me.name, action, "refused admin call");
    Err(ApiError::new(
        Code::NotAdmin,
        format!("agent {} is not an admin", me.name),
        "Only an agent marked `admin = true` in the server config may use /admin routes; ask the owner.",
    ))
}

/// Runs an account change and audits how it ended, by error code when it failed.
pub(super) fn audited<T>(
    app: &App,
    me: &Caller,
    action: &str,
    target: &str,
    run: impl FnOnce() -> ApiResult<T>,
) -> ApiResult<T> {
    let result = run();
    let outcome = match &result {
        Ok(_) => "ok",
        Err(e) => e.code.as_str(),
    };
    record(app, me.name.as_str(), action, Some(target), outcome);
    tracing::info!(target: "racking::admin", actor = %me.name, action, target, result = outcome);
    result
}

fn name_param(call: &Call<'_>) -> ApiResult<AgentName> {
    call.param(0)
}

pub(super) fn list_agents(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    admin(app, call, "agent.list", None)?;
    let agents: Vec<Value> = app
        .registry
        .list()
        .into_iter()
        .map(|a| {
            json!({
                "name": a.name.as_str(),
                "role": a.role.as_str(),
                "admin": a.admin,
                "source": a.source.as_str(),
                "token_created_at": a.token_created_at.map(clock::rfc3339),
                "last_used": app.presence.last_seen(&a.name).map(clock::rfc3339),
                "pending": a.pending,
            })
        })
        .collect();
    ok(json!({ "agents": agents }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAgentBody {
    name: String,
    role: String,
}

pub(super) fn create_agent(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let input: NewAgentBody = call.body()?;
    let me = admin(app, call, "agent.add", Some(truncate(&input.name, 40)))?;
    let name: AgentName = input
        .name
        .parse()
        .map_err(|e: String| ApiError::bad_request(format!("name: {e}")))?;
    let role: Role = input
        .role
        .parse()
        .map_err(|e: String| ApiError::bad_request(format!("role: {e}")))?;
    let issued = audited(app, me, "agent.add", name.as_str(), || {
        if app.store.name_reserved(&name, app.clock.now())? {
            return Err(ApiError::new(
                Code::Conflict,
                format!("an open invite will make an agent named {name}"),
                "Pick another name, or cancel that invite (DELETE /admin/invites/{id}).",
            ));
        }
        app.accounts().create(&name, role)
    })?;
    Ok(Reply::Json {
        status: 201,
        body: json!({
            "name": issued.name.as_str(),
            "role": issued.role.as_str(),
            "token": issued.token.as_str(),
            "note": "This token is shown once and stored only as a hash. Give it to the agent's owner now.",
        }),
    })
}

pub(super) fn rotate_agent(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let name = name_param(call)?;
    let me = admin(app, call, "agent.rotate", Some(name.as_str()))?;
    let issued = audited(app, me, "agent.rotate", name.as_str(), || app.accounts().rotate(&name))?;
    ok(json!({
        "name": issued.name.as_str(),
        "role": issued.role.as_str(),
        "token": issued.token.as_str(),
        "note": "The old token no longer works. This one is shown once and stored only as a hash.",
    }))
}

pub(super) fn remove_agent(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let name = name_param(call)?;
    let me = admin(app, call, "agent.remove", Some(name.as_str()))?;
    audited(app, me, "agent.remove", name.as_str(), || app.accounts().revoke(&name))?;
    ok(json!({"name": name.as_str(), "removed": true}))
}

pub(super) fn audit(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    admin(app, call, "audit.read", None)?;
    let limit = match call.query_raw("limit") {
        None => limits::LIST_DEFAULT,
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=limits::LIST_MAX).contains(n))
            .ok_or_else(|| ApiError::bad_request(format!("limit: 1 to {}", limits::LIST_MAX)))?,
    };
    let before = call
        .query_raw("before")
        .map(|t| {
            t.parse::<i64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| ApiError::bad_request("before: a positive entry id"))
        })
        .transpose()?;
    ok(app.store.audit_page(before, limit)?)
}

pub(super) fn pause(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    set_brake(app, call, true)
}

pub(super) fn resume(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    set_brake(app, call, false)
}

fn set_brake(app: &App, call: &mut Call<'_>, on: bool) -> ApiResult<Reply> {
    let action = if on { "pause" } else { "resume" };
    let me = admin(app, call, action, None)?;
    // The store first, so a flag the clients saw always survives a restart.
    audited(app, me, action, "server", || app.store.set_paused(on))?;
    app.paused.store(on, Ordering::Relaxed);
    if on {
        tracing::warn!(target: "racking::admin", actor = %me.name, "emergency brake pulled: workers cannot send");
    }
    ok(json!({ "paused": on }))
}
