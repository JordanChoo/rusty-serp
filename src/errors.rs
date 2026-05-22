use worker::*;

const MAX_BODY_BYTES: usize = 4096;

pub fn json_error(status: u16, error: &str, code: &str) -> Result<Response> {
    let body = serde_json::json!({
        "error": error,
        "code": code,
    });
    let mut resp = Response::ok(body.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp.with_status(status))
}

pub fn dataforseo_error(dfs_status: u16, dfs_body: &str, dfs_error_code: Option<&str>) -> Result<Response> {
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
