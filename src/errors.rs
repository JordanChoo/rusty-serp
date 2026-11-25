use worker::*;

const MAX_BODY_BYTES: usize = 4096;
/// `Retry-After` for a rate-limited call, in seconds: DataForSEO's per-minute
/// limit clears within a minute.
const RATE_LIMIT_RETRY_AFTER_SECONDS: &str = "60";

pub fn json_error(status: u16, error: &str, code: &str) -> Result<Response> {
    let body = serde_json::json!({
        "error": error,
        "code": code,
    });
    let mut resp = Response::ok(body.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp.with_status(status))
}

pub fn dataforseo_error(
    dfs_status: u16,
    dfs_body: &str,
    dfs_error_code: Option<&str>,
) -> Result<Response> {
    let mut payload = serde_json::json!({
        "error": "DataForSEO request failed",
        "code": "dataforseo_error",
        "dataforseo_status": dfs_status,
        "dataforseo_body": truncate(dfs_body, MAX_BODY_BYTES),
    });

    if let Some(error_code) = dfs_error_code {
        payload["dataforseo_error_code"] = serde_json::json!(error_code);
    }

    let mut resp = Response::ok(payload.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp.with_status(502))
}

/// The task did not finish before the Worker's deadline. DataForSEO may still
/// finish it and charge for it; the task ID lets someone look it up.
pub fn task_timeout_error(task_id: &str) -> Result<Response> {
    let payload = serde_json::json!({
        "error": "DataForSEO task did not finish in time",
        "code": "dataforseo_timeout",
        "dataforseo_task_id": task_id,
    });

    let mut resp = Response::ok(payload.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp.with_status(504))
}

/// DataForSEO's rate limits turned the task away for longer than the Worker
/// keeps resending it. No task was set and nothing was paid, so the caller can
/// safely try again later. `dfs_body` is DataForSEO's last task_post response.
pub fn rate_limited_error(dfs_body: &str) -> Result<Response> {
    let payload = serde_json::json!({
        "error": "DataForSEO rate limit: no task was set, try again later",
        "code": "dataforseo_rate_limited",
        "dataforseo_body": truncate(dfs_body, MAX_BODY_BYTES),
    });

    let mut resp = Response::ok(payload.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    resp.headers_mut()
        .set("Retry-After", RATE_LIMIT_RETRY_AFTER_SECONDS)?;
    Ok(resp.with_status(429))
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
