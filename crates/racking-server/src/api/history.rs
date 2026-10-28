//! `GET /threads/{id}` and `GET /jobs`: reading past conversations. Both are
//! peeks, so they deliver nothing; their parameters are checked here, once.

use super::{App, Call, Reply, ok};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::{SearchQuery, ThreadQuery, View, limits};
use crate::types::{AgentName, Cursor, JobId, UtcTime};

pub(super) const THREAD_QUERY: &[(&str, &str)] = &[
    (
        "full",
        "`1` gives every job its whole body (pages are cut at 1 MiB); default `0`, summaries as in `/inbox`",
    ),
    ("limit", "jobs per page, 1 to 200 (default 50)"),
    ("after", "cursor: the `next` of the previous page"),
];

pub(super) const SEARCH_QUERY: &[(&str, &str)] = &[
    (
        "q",
        "text found anywhere in the title or body, 1 to 100 chars, case-insensitive for ASCII letters; \
         `%` and `_` match themselves; URL-encode it",
    ),
    ("from", "only jobs this agent sent"),
    ("to", "only jobs sent to this agent"),
    (
        "kind",
        "only this kind (`task`, `message`, `notice`, `report`, `comment`, `result`)",
    ),
    ("state", "only jobs in this state"),
    (
        "since",
        "created at or after this UTC instant: `YYYY-MM-DD` (midnight) or `YYYY-MM-DDTHH:MM:SSZ`",
    ),
    ("until", "created before this UTC instant, same forms as `since`"),
    ("limit", "page size, 1 to 200 (default 50)"),
    ("after", "cursor: the `next` of the previous page"),
];

/// A query value with `%XX` and `+` decoded, so text with spaces and non-ASCII
/// letters arrives as typed. A malformed escape or non-UTF-8 result is refused.
fn decoded(call: &Call<'_>, name: &str) -> ApiResult<Option<String>> {
    let Some(raw) = call.query_raw(name) else {
        return Ok(None);
    };
    let bad = || {
        ApiError::new(
            Code::BadRequest,
            format!("{name}: not valid URL encoding"),
            "Percent-encode the value as UTF-8 (a space as %20, `%` as %25).",
        )
    };
    let mut bytes = Vec::with_capacity(raw.len());
    let mut it = raw.bytes();
    while let Some(b) = it.next() {
        match b {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hex = [it.next().ok_or_else(bad)?, it.next().ok_or_else(bad)?];
                let text = std::str::from_utf8(&hex).map_err(|_| bad())?;
                bytes.push(u8::from_str_radix(text, 16).map_err(|_| bad())?);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).map(Some).map_err(|_| bad())
}

/// A decoded value parsed into its validated type, with the parameter's name in the error.
fn parsed<T: std::str::FromStr<Err = String>>(call: &Call<'_>, name: &str, hint: &str) -> ApiResult<Option<T>> {
    decoded(call, name)?
        .map(|v| {
            v.parse()
                .map_err(|e: String| ApiError::new(Code::BadRequest, format!("{name}: {e}"), hint))
        })
        .transpose()
}

fn limit(call: &Call<'_>) -> ApiResult<u32> {
    match call.query_raw("limit") {
        None => Ok(limits::LIST_DEFAULT),
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=limits::LIST_MAX).contains(n))
            .ok_or_else(|| {
                ApiError::new(
                    Code::BadRequest,
                    format!("limit: 1 to {}", limits::LIST_MAX),
                    "Leave it out for 50; page on with `after`.",
                )
            }),
    }
}

const CURSOR_HINT: &str = "Pass `next` from the previous answer unchanged, or leave `after` out to start over.";

fn thread_query(call: &Call<'_>) -> ApiResult<(JobId, ThreadQuery)> {
    let id: JobId = call.param(0)?;
    let view = match call.query_raw("full") {
        None | Some("0") => View::Summary,
        Some("1") => View::Full,
        Some(_) => {
            return Err(ApiError::new(
                Code::BadRequest,
                "full: 1 or 0",
                "`full=1` for whole bodies; leave it out for summaries.",
            ));
        }
    };
    let query = ThreadQuery {
        view,
        limit: limit(call)?,
        after: parsed::<Cursor>(call, "after", CURSOR_HINT)?,
    };
    Ok((id, query))
}

fn search_query(call: &Call<'_>) -> ApiResult<SearchQuery> {
    let text = decoded(call, "q")?;
    if let Some(t) = &text
        && (t.trim().is_empty() || t.chars().count() > limits::SEARCH_CHARS)
    {
        return Err(ApiError::new(
            Code::BadRequest,
            format!("q: 1 to {} chars, not blank", limits::SEARCH_CHARS),
            "Search a distinctive word or phrase; leave `q` out to list by the other filters only.",
        ));
    }
    let agent_hint = "An agent name as GET /agents lists it.";
    let time_hint = "A UTC date `2026-10-28` or time `2026-10-28T12:00:00Z`.";
    let since = parsed::<UtcTime>(call, "since", time_hint)?.map(UtcTime::get);
    let until = parsed::<UtcTime>(call, "until", time_hint)?.map(UtcTime::get);
    if let (Some(s), Some(u)) = (since, until)
        && s >= u
    {
        return Err(ApiError::new(
            Code::BadRequest,
            "since: must be before until",
            "`since` is inclusive and `until` exclusive: give an earlier `since`.",
        ));
    }
    Ok(SearchQuery {
        text,
        from: parsed::<AgentName>(call, "from", agent_hint)?,
        to: parsed::<AgentName>(call, "to", agent_hint)?,
        kind: parsed(call, "kind", "One of task, message, notice, report, comment, result.")?,
        state: parsed(
            call,
            "state",
            "One of sent, received, accepted, rejected, done, failed, answered.",
        )?,
        since,
        until,
        limit: limit(call)?,
        after: parsed::<Cursor>(call, "after", CURSOR_HINT)?,
    })
}

pub(super) fn thread(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let (id, query) = thread_query(call)?;
    let me = call.me()?;
    let page = app.store.thread(me, id, &query)?;
    tracing::debug!(
        target: "racking::history",
        agent = %me,
        job = %id,
        jobs = page.jobs.len(),
        omitted = page.omitted,
        "thread read"
    );
    ok(page.to_json())
}

pub(super) fn search(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let query = search_query(call)?;
    let me = call.me()?;
    let page = app.store.search(me, &query)?;
    tracing::debug!(
        target: "racking::history",
        agent = %me,
        by_text = query.text.is_some(),
        jobs = page.jobs.len(),
        "search"
    );
    ok(page.to_json())
}
