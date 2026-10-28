//! The one error shape of the API: `{"error": {"code", "message", "hint"}}`.

use serde_json::json;

/// Error codes of the protocol. Each maps to exactly one HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    BadRequest,
    NoToken,
    BadToken,
    NotYours,
    NotAdmin,
    NotAllowed,
    NotFound,
    BadTransition,
    TooDeep,
    AttachmentsLocked,
    TooLarge,
    ChecksumMismatch,
    IdempotencyConflict,
    Conflict,
    RateLimited,
    Internal,
    Paused,
}

impl Code {
    /// Every code with its status and meaning, in the order `/help` lists them.
    pub const ALL: [(Code, &'static str); 17] = [
        (
            Code::BadRequest,
            "Malformed JSON or field out of range. `message` names the field.",
        ),
        (Code::NoToken, "Missing `Authorization: Bearer` header."),
        (Code::BadToken, "Unknown token."),
        (
            Code::NotYours,
            "The job belongs to other agents, or the move is the other party's.",
        ),
        (
            Code::NotAdmin,
            "Publishing `rack` needs an agent with `admin = true` in the server config.",
        ),
        (
            Code::NotAllowed,
            "Your role or the `[permissions]` of the server config do not let you send to that agent. The hint \
             names who you can send to.",
        ),
        (
            Code::NotFound,
            "No such job, attachment, agent, published binary or route.",
        ),
        (
            Code::BadTransition,
            "State move not allowed, e.g. accepting a message or replying to a notice.",
        ),
        (
            Code::TooDeep,
            "The job would be more than 3 hops from the job that started its chain (a task sent as a reply, or a \
             job with `parent`). Answer the original sender instead of starting another task.",
        ),
        (
            Code::AttachmentsLocked,
            "The job is no longer `sent`, attachments are fixed; or it is a notice, which carries no files.",
        ),
        (Code::TooLarge, "Body or file over the limit. The hint names the limit."),
        (Code::ChecksumMismatch, "`X-Sha256` differs from the uploaded bytes."),
        (
            Code::IdempotencyConflict,
            "Same `Idempotency-Key` used for a different request.",
        ),
        (
            Code::Conflict,
            "Admin API: the agent name is taken, or a second main agent was asked for.",
        ),
        (
            Code::RateLimited,
            "Too many requests in flight, too many jobs this minute, or too many open tasks to one agent. Wait \
             `Retry-After` seconds.",
        ),
        (Code::Internal, "Storage or disk failure on the server."),
        (
            Code::Paused,
            "The owner pulled the emergency brake (`POST /admin/pause`): workers cannot send until it is \
             resumed. Stop acting on jobs.",
        ),
    ];

    pub fn status(self) -> u16 {
        match self {
            Code::BadRequest => 400,
            Code::NoToken | Code::BadToken => 401,
            Code::NotYours | Code::NotAdmin | Code::NotAllowed => 403,
            Code::NotFound => 404,
            Code::BadTransition | Code::TooDeep | Code::AttachmentsLocked | Code::Conflict => 409,
            Code::TooLarge => 413,
            Code::ChecksumMismatch | Code::IdempotencyConflict => 422,
            Code::RateLimited => 429,
            Code::Internal => 500,
            Code::Paused => 503,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Code::BadRequest => "bad_request",
            Code::NoToken => "no_token",
            Code::BadToken => "bad_token",
            Code::NotYours => "not_yours",
            Code::NotAdmin => "not_admin",
            Code::NotAllowed => "not_allowed",
            Code::NotFound => "not_found",
            Code::BadTransition => "bad_transition",
            Code::TooDeep => "too_deep",
            Code::AttachmentsLocked => "attachments_locked",
            Code::TooLarge => "too_large",
            Code::ChecksumMismatch => "checksum_mismatch",
            Code::IdempotencyConflict => "idempotency_conflict",
            Code::RateLimited => "rate_limited",
            Code::Internal => "internal",
            Code::Paused => "paused",
            Code::Conflict => "conflict",
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub code: Code,
    pub message: String,
    pub hint: String,
    /// Seconds for the `Retry-After` header of a 429 or 503.
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: hint.into(),
            retry_after: None,
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message, "See GET /help for the fields and limits.")
    }

    /// Storage failures carry no detail to the client, so paths and SQL never leak;
    /// the cause goes to the server log instead.
    pub fn internal(context: &str, cause: &dyn std::fmt::Display) -> Self {
        tracing::error!(target: "racking::store", context, %cause, "internal error");
        Self::new(
            Code::Internal,
            format!("server failed to {context}"),
            "Retry later; the owner can read the server log.",
        )
    }

    pub fn body(&self) -> serde_json::Value {
        json!({"error": {"code": self.code.as_str(), "message": self.message, "hint": self.hint}})
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal("access the database", &e)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
