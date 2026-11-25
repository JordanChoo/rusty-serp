use crate::errors::{dataforseo_error, json_error, task_timeout_error};
use crate::validation::{Location, SerpRequest};
use base64::Engine;
use std::time::Duration;
use worker::*;

const TASK_POST_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/task_post";
const TASK_GET_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced";

/// DataForSEO's high execution priority. Normal-priority tasks wait in a queue
/// and can take minutes; high-priority ones start at once and cost more per page.
const HIGH_PRIORITY: u8 = 2;

// Cloudflare ends a Worker's outbound request with a 524 when the server sends
// nothing for 125 seconds, and a task with redirect resolution and the
// poisoned-result check can take longer than that. So the Worker sets the task
// and then asks for its result every few seconds. Each of those requests
// returns at once; only the Worker's answer to its own caller takes long.

/// Wait before the first result request. Tasks with both checks on take over a
/// minute, so asking sooner only spends requests.
const FIRST_POLL_AFTER_MS: u64 = 10_000;
/// Wait between result requests.
const POLL_INTERVAL_MS: u64 = 6_000;
/// Overall limit for one call, counted from just before the task is set. It
/// leaves room under the 300-second client timeout the README recommends, so a
/// caller gets a 504 from the Worker rather than its own timeout.
const DEADLINE_MS: u64 = 280_000;
/// Failed result requests in a row (network error, non-2xx status, body that
/// is not JSON) before the Worker gives up on the task.
const MAX_POLL_FAILURES: u32 = 3;
/// Most outbound requests one call can make: the task_post request plus every
/// result request the deadline allows.
const MAX_SUBREQUESTS: u64 = 1 + (DEADLINE_MS - FIRST_POLL_AFTER_MS) / POLL_INTERVAL_MS + 1;

// Checked when compiling: Workers Free allows 50 subrequests per invocation,
// and the Worker must answer before the 300-second client timeout.
const _: () = assert!(MAX_SUBREQUESTS <= 50);
const _: () = assert!(DEADLINE_MS + POLL_INTERVAL_MS < 300_000);

/// task_post status code for a task that was created.
const TASK_CREATED: i64 = 20100;
/// task_get status codes for a task that has not finished: 40601 "Task Handed"
/// (received, not yet queued) and 40602 "Task In Queue".
const PENDING_STATUS_CODES: [i64; 2] = [40601, 40602];

/// Builds the DataForSEO task_post body: an array holding one task.
///
/// The task runs at high priority (`priority: 2`). Three flags are always on.
/// `SerpRequest` has no field for any of them, so callers cannot turn them off:
/// - `load_async_ai_overview` fetches AI Overviews that Google loads late.
/// - `resolve_goto_urls` makes DataForSEO replace Google redirect links in the
///   results with the final destination URLs (in testing, both `url` fields and
///   links inside `markdown` text). It must be set explicitly: for
///   high-priority tasks it defaults to off. DataForSEO says it adds up to 40
///   seconds, for a turnaround of at most 260 seconds.
/// - `resolve_poisoned_results` makes DataForSEO check the organic results
///   against Google's Web view and replace poisoned listings with verified
///   ones, marked `from_web` in their `checks` array. It adds the price of one
///   SERP page to every task, whether or not anything was poisoned.
fn build_payload(request: &SerpRequest) -> serde_json::Value {
    let mut task = serde_json::json!({
        "keyword": request.keyword,
        "depth": request.depth,
        "device": request.device,
        "language_code": request.language,
        "priority": HIGH_PRIORITY,
        "load_async_ai_overview": true,
        "resolve_goto_urls": true,
        "resolve_poisoned_results": true,
    });

    match &request.location {
        Location::Code(code) => {
            task["location_code"] = serde_json::json!(code);
        }
        Location::Name(name) => {
            task["location_name"] = serde_json::json!(name);
        }
    }

    serde_json::json!([task])
}

/// The task ID from a task_post response, if DataForSEO created the task.
fn created_task_id(body: &serde_json::Value) -> Option<&str> {
    let task = body.get("tasks")?.get(0)?;
    if task.get("status_code")?.as_i64()? != TASK_CREATED {
        return None;
    }
    task.get("id")?.as_str()
}

/// The task_get URL for a task, in the `.ai` format when `ai_optimized` is set.
///
/// Returns `None` unless the ID looks like the UUID DataForSEO issues, so that
/// nothing else from the response ends up in the URL path.
fn task_get_url(task_id: &str, ai_optimized: bool) -> Option<String> {
    let valid = task_id.len() == 36 && task_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !valid {
        return None;
    }
    let suffix = if ai_optimized { ".ai" } else { "" };
    Some(format!("{TASK_GET_URL}/{task_id}{suffix}"))
}

/// Whether a task_get response says the task has not finished yet.
///
/// The standard format carries the task's status in `tasks[0].status_code`;
/// the `.ai` format puts it in the top-level `status_code`. A response without
/// a status code counts as finished and is passed through to the caller.
fn is_pending(body: &serde_json::Value, ai_optimized: bool) -> bool {
    let status = if ai_optimized {
        body.get("status_code")
    } else {
        body.get("tasks")
            .and_then(|tasks| tasks.get(0))
            .and_then(|task| task.get("status_code"))
    };
    status
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|code| PENDING_STATUS_CODES.contains(&code))
}

/// Sends one request to DataForSEO and returns its status and body text.
async fn send(
    url: &str,
    method: Method,
    auth_header: &str,
    body: Option<String>,
) -> std::result::Result<(u16, String), Response> {
    let headers = Headers::new();
    headers.set("Authorization", auth_header).map_err(|_| {
        json_error(500, "Failed to build request headers", "missing_config").unwrap()
    })?;
    if body.is_some() {
        headers
            .set("Content-Type", "application/json")
            .map_err(|_| {
                json_error(500, "Failed to build request headers", "missing_config").unwrap()
            })?;
    }

    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(body) = body {
        init.with_body(Some(body.into()));
    }

    let outbound = Request::new_with_init(url, &init).map_err(|_| {
        json_error(500, "Failed to build outbound request", "missing_config").unwrap()
    })?;

    let mut response = Fetch::Request(outbound).send().await.map_err(|e| {
        let err_msg = format!("Fetch failed: {}", e);
        let err_lower = err_msg.to_lowercase();
        if err_lower.contains("timeout") || err_lower.contains("timed out") {
            json_error(504, &err_msg, "dataforseo_timeout").unwrap()
        } else {
            dataforseo_error(0, &err_msg, None).unwrap()
        }
    })?;

    let status = response.status_code();
    let body_text = response.text().await.map_err(|e| {
        dataforseo_error(status, &format!("Failed to read response: {}", e), None).unwrap()
    })?;
    Ok((status, body_text))
}

/// Asks DataForSEO for the task's result once.
///
/// `Ok(Some(body))` is the finished task's response, `Ok(None)` means the task
/// is still running, and `Err` is a failed request, which the caller may retry.
async fn poll_once(
    url: &str,
    auth_header: &str,
    ai_optimized: bool,
) -> std::result::Result<Option<String>, Response> {
    let (status, body_text) = send(url, Method::Get, auth_header, None).await?;
    if !(200..300).contains(&status) {
        return Err(dataforseo_error(status, &body_text, None).unwrap());
    }
    let body: serde_json::Value = serde_json::from_str(&body_text)
        .map_err(|_| dataforseo_error(status, &body_text, None).unwrap())?;
    Ok((!is_pending(&body, ai_optimized)).then_some(body_text))
}

/// Wraps a DataForSEO response body, unchanged, in a `200` JSON response.
fn pass_through(body_text: String) -> std::result::Result<Response, Response> {
    let mut resp = Response::ok(body_text)
        .map_err(|_| dataforseo_error(200, "Failed to construct response", None).unwrap())?;
    resp.headers_mut()
        .set("Content-Type", "application/json")
        .map_err(|_| dataforseo_error(200, "Failed to set response headers", None).unwrap())?;
    Ok(resp)
}

/// Runs one SERP query: sets a high-priority task, then polls for its result.
///
/// The finished task's task_get response is returned verbatim. So is a
/// task_post response whose task was not created (an invalid field or an empty
/// balance, for example), because it carries DataForSEO's error in its
/// `status_code` the same way.
pub async fn fetch_serp(
    request: &SerpRequest,
    login: &str,
    password: &str,
) -> std::result::Result<Response, Response> {
    let credentials = format!("{}:{}", login, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    let auth_header = format!("Basic {}", encoded);

    let started_ms = Date::now().as_millis();
    let payload = build_payload(request).to_string();
    let (status, post_text) =
        send(TASK_POST_URL, Method::Post, &auth_header, Some(payload)).await?;
    if !(200..300).contains(&status) {
        return Err(dataforseo_error(status, &post_text, None).unwrap());
    }
    let post_body: serde_json::Value = serde_json::from_str(&post_text)
        .map_err(|_| dataforseo_error(status, &post_text, None).unwrap())?;

    let Some(task_id) = created_task_id(&post_body) else {
        return pass_through(post_text);
    };
    let Some(task_url) = task_get_url(task_id, request.ai_optimized) else {
        return Err(dataforseo_error(status, &post_text, None).unwrap());
    };

    Delay::from(Duration::from_millis(FIRST_POLL_AFTER_MS)).await;
    let mut failures = 0;
    loop {
        match poll_once(&task_url, &auth_header, request.ai_optimized).await {
            Ok(Some(body_text)) => return pass_through(body_text),
            Ok(None) => failures = 0,
            Err(error) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Err(error);
                }
            }
        }

        let elapsed_ms = Date::now().as_millis().saturating_sub(started_ms);
        if elapsed_ms + POLL_INTERVAL_MS > DEADLINE_MS {
            return Err(task_timeout_error(task_id).unwrap());
        }
        Delay::from(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(location: Location, ai_optimized: bool) -> SerpRequest {
        SerpRequest {
            keyword: "rust programming".to_string(),
            location,
            depth: 10,
            device: "desktop".to_string(),
            language: "en".to_string(),
            ai_optimized,
        }
    }

    const TASK_ID: &str = "11260820-1535-0139-0000-7b3c1d2e4f5a";

    #[test]
    fn always_on_flags_are_set_for_both_endpoints_and_location_types() {
        for ai_optimized in [false, true] {
            for location in [
                Location::Code(2840),
                Location::Name("United States".to_string()),
            ] {
                let payload = build_payload(&request(location, ai_optimized));
                let tasks = payload.as_array().expect("payload must be an array");
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0]["priority"], 2);
                assert_eq!(tasks[0]["resolve_goto_urls"], true);
                assert_eq!(tasks[0]["load_async_ai_overview"], true);
                assert_eq!(tasks[0]["resolve_poisoned_results"], true);
            }
        }
    }

    #[test]
    fn task_id_is_read_only_from_a_created_task() {
        let created = serde_json::json!({
            "status_code": 20000,
            "tasks": [{"id": TASK_ID, "status_code": 20100, "status_message": "Task Created."}],
        });
        assert_eq!(created_task_id(&created), Some(TASK_ID));

        let invalid_field = serde_json::json!({
            "status_code": 20000,
            "tasks": [{"id": TASK_ID, "status_code": 40501, "status_message": "Invalid Field"}],
        });
        assert_eq!(created_task_id(&invalid_field), None);

        let no_tasks = serde_json::json!({"status_code": 40100, "tasks": null});
        assert_eq!(created_task_id(&no_tasks), None);
    }

    #[test]
    fn task_get_url_matches_the_format_and_rejects_odd_ids() {
        assert_eq!(
            task_get_url(TASK_ID, false).as_deref(),
            Some("https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced/11260820-1535-0139-0000-7b3c1d2e4f5a")
        );
        assert_eq!(
            task_get_url(TASK_ID, true).as_deref(),
            Some("https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced/11260820-1535-0139-0000-7b3c1d2e4f5a.ai")
        );
        assert_eq!(task_get_url("../live/advanced", false), None);
        assert_eq!(task_get_url("", false), None);
    }

    #[test]
    fn pending_is_read_from_the_right_field_for_each_format() {
        for code in [40601, 40602] {
            let standard =
                serde_json::json!({"status_code": 20000, "tasks": [{"status_code": code}]});
            assert!(is_pending(&standard, false));
            let ai = serde_json::json!({"id": TASK_ID, "status_code": code, "items": []});
            assert!(is_pending(&ai, true));
        }

        let done = serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 20000}]});
        assert!(!is_pending(&done, false));
        let done_ai = serde_json::json!({"id": TASK_ID, "status_code": 20000, "items": []});
        assert!(!is_pending(&done_ai, true));

        let failed = serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 40102}]});
        assert!(!is_pending(&failed, false));
        let no_status = serde_json::json!({"status_code": 50000, "tasks": null});
        assert!(!is_pending(&no_status, false));
    }
}
