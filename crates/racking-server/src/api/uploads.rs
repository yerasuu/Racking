//! Resumable uploads at the HTTP edge: the request fields and headers are
//! checked here, once, and the store does the rest.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{App, Call, Reply, ok};
use crate::error::{ApiError, ApiResult, Code};
use crate::store::{NewUpload, Opened};
use crate::types::{AttachmentName, ContentRange, JobId, Sha256Hex, UploadId, truncate};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUploadBody {
    name: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    content_type: Option<String>,
}

pub(super) fn create(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: JobId = call.param(0)?;
    let input: CreateUploadBody = call.body()?;
    let new = NewUpload {
        name: input
            .name
            .parse::<AttachmentName>()
            .map_err(|e| ApiError::bad_request(format!("name: {e}")))?,
        size: input.size,
        sha256: input
            .sha256
            .parse::<Sha256Hex>()
            .map_err(|_| ApiError::bad_request("sha256 must be 64 hex characters"))?,
        content_type: input.content_type.map(|c| truncate(&c, 100).to_owned()),
    };
    let (session, opened) = app.store.create_upload(call.me()?, id, new)?;
    let status = match opened {
        Opened::Created => 201,
        Opened::Resumed => 200,
    };
    Ok(Reply::Json {
        status,
        body: session.to_json(app.store.upload_rules().chunk_bytes),
    })
}

pub(super) fn list(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: JobId = call.param(0)?;
    let chunk = app.store.upload_rules().chunk_bytes;
    let sessions = app.store.uploads_of(call.me()?, id)?;
    ok(json!({"uploads": sessions.iter().map(|s| s.to_json(chunk)).collect::<Vec<Value>>()}))
}

pub(super) fn get(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: UploadId = call.param(0)?;
    let session = app.store.upload(call.me()?, id)?;
    ok(session.to_json(app.store.upload_rules().chunk_bytes))
}

pub(super) fn put_chunk(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: UploadId = call.param(0)?;
    let range: ContentRange = call
        .header("Content-Range")
        .ok_or_else(|| {
            ApiError::new(
                Code::BadRequest,
                "Content-Range header is required",
                "Send `Content-Range: bytes FIRST-LAST/SIZE` with FIRST the upload's `received` offset.",
            )
        })?
        .parse()
        .map_err(ApiError::bad_request)?;
    // The body must be exactly the range, so a cut connection shows as fewer bytes, never as a shorter range.
    match call.request.body_length() {
        Some(n) if n as u64 == range.length() => {}
        Some(n) => {
            return Err(ApiError::bad_request(format!(
                "Content-Length {n} differs from the {} bytes of Content-Range",
                range.length()
            )));
        }
        None => {
            return Err(ApiError::new(
                Code::BadRequest,
                "Content-Length header is required",
                "Send each chunk with its length, not chunked transfer encoding.",
            ));
        }
    }
    let me = call.me()?.clone();
    let session = app.store.put_chunk(&me, id, range, call.request.as_reader())?;
    ok(session.to_json(app.store.upload_rules().chunk_bytes))
}

pub(super) fn complete(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: UploadId = call.param(0)?;
    let (session, attachment) = app.store.complete_upload(call.me()?, id)?;
    let mut body = session.to_json(app.store.upload_rules().chunk_bytes);
    body["attachment"] = attachment.to_json();
    ok(body)
}

pub(super) fn abort(app: &App, call: &mut Call<'_>) -> ApiResult<Reply> {
    let id: UploadId = call.param(0)?;
    app.store.abort_upload(call.me()?, id)?;
    ok(json!({"upload_id": id.to_string(), "aborted": true}))
}
