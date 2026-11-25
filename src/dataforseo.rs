use crate::errors::{dataforseo_error, json_error, task_timeout_error};
use crate::validation::{Location, SerpRequest};
use base64::Engine;
use serde::Deserialize;
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
/// Overall limit for one call, counted from just before the task is set. No
/// result request starts after it. It leaves room under the 300-second client
/// timeout the README recommends, so a caller gets a 504 from the Worker rather
/// than its own timeout.
const DEADLINE_MS: u64 = 280_000;
/// Longest wait for one result request, body included. These normally take well
/// under a second; one that hangs is abandoned and counts as a failure, so it
/// cannot hold the Worker's answer past the client timeout. (The task_post
/// request has no limit of its own: abandoning it could leave a paid task
/// whose ID nobody has. Cloudflare's cutoff, `CLOUDFLARE_CUTOFF_MS`, bounds it.)
const POLL_TIMEOUT_MS: u32 = 10_000;
/// Failed result requests in a row (network error, timeout, non-2xx status,
/// unreadable body, DataForSEO server error) before the Worker gives up on the
/// task.
const MAX_POLL_FAILURES: u32 = 3;
/// A task_post that DataForSEO's rate limits turn away is sent again every
/// `POLL_INTERVAL_MS`, as long as the next attempt starts within this time of
/// the first (counted from the same moment as the deadline). Such a request set
/// no task and cost nothing. A minute outlasts the per-minute limit and still
/// leaves the task about 220 seconds before the deadline.
const POST_RETRY_WINDOW_MS: u64 = 60_000;
/// Cloudflare's cutoff for an outbound request whose server sends nothing.
const CLOUDFLARE_CUTOFF_MS: u64 = 125_000;
/// Most outbound requests one call can make. Each starts at least
/// `POLL_INTERVAL_MS` after the one before (a resent task_post waits that long,
/// and the first result request waits `FIRST_POLL_AFTER_MS`, which is longer),
/// and none starts after the deadline.
const MAX_SUBREQUESTS: u64 = 1 + DEADLINE_MS / POLL_INTERVAL_MS;

// Checked when compiling: Workers Free allows 50 subrequests per invocation,
// and the Worker's answer must come at least 10 seconds before the 300-second
// client timeout, leaving time for the request's travel and the download. At
// worst that answer follows one result request that starts at the deadline and
// times out, or a task_post that starts at the end of its retry window and
// runs into Cloudflare's cutoff.
const _: () = assert!(FIRST_POLL_AFTER_MS >= POLL_INTERVAL_MS);
const _: () = assert!(POST_RETRY_WINDOW_MS <= DEADLINE_MS);
const _: () = assert!(MAX_SUBREQUESTS <= 50);
const _: () = assert!(DEADLINE_MS + POLL_TIMEOUT_MS as u64 <= 290_000);
const _: () = assert!(POST_RETRY_WINDOW_MS + CLOUDFLARE_CUTOFF_MS <= 290_000);

/// task_post status code for a task that was created.
const TASK_CREATED: i64 = 20100;
/// task_get status codes for a task that has not finished: 40601 "Task Handed"
/// (received, not yet queued) and 40602 "Task In Queue".
const PENDING_STATUS_CODES: [i64; 2] = [40601, 40602];
/// Status codes with which DataForSEO turns a request away for the moment:
/// 40202 (too many requests per minute) and 40209 (more than 30 simultaneous
/// requests, counted across everything that uses the account). They come with
/// HTTP 200 and say nothing about the task, so a result request that gets one
/// is treated like a pending task: the Worker waits and asks again, until the
/// deadline. A task_post that gets one is sent again for a while (see
/// `POST_RETRY_WINDOW_MS`). DataForSEO does not say where in the body they
/// appear, and no finished task carries either one, so both the top-level code
/// and `tasks[0]`'s are read.
const BUSY_STATUS_CODES: [i64; 2] = [40202, 40209];
/// DataForSEO reports its own server errors as HTTP 200 with a `status_code`
/// from 50000 up. A result request that gets one counts as failed, since the
/// task may still finish. (In the `.ai` format the top-level code is also the
/// task's own, so a task that itself ended in such an error is asked for again
/// too, and after three tries ends as a 502 rather than a 200.)
const FIRST_SERVER_ERROR: i64 = 50000;

/// The status codes of a DataForSEO response, and nothing else: a task_get
/// response, or a task_post response being checked for a rate limit.
///
/// Deserializing into this skips the rest of the body without building it, so
/// a large result costs little CPU time. For a 1.9 MB result (about depth 200)
/// that took a tenth of the time of parsing it whole, in a native benchmark.
#[derive(Deserialize)]
struct StatusCodes {
    status_code: Option<i64>,
    tasks: Option<Vec<TaskStatusCode>>,
}

#[derive(Deserialize)]
struct TaskStatusCode {
    status_code: Option<i64>,
}

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

/// Whether DataForSEO's rate limits turned a task_post request away. It then
/// set no task, so it can be sent again without paying twice.
fn post_is_busy(body: &serde_json::Value) -> bool {
    created_task_id(body).is_none()
        && StatusCodes::deserialize(body).is_ok_and(|codes| is_busy(&codes))
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
fn is_pending(codes: &StatusCodes, ai_optimized: bool) -> bool {
    let status = if ai_optimized {
        codes.status_code
    } else {
        codes
            .tasks
            .as_ref()
            .and_then(|tasks| tasks.first())
            .and_then(|task| task.status_code)
    };
    status.is_some_and(|code| PENDING_STATUS_CODES.contains(&code))
}

/// Whether DataForSEO turned a request away for the moment (a rate limit)
/// instead of reporting on the task. The top-level `status_code` and
/// `tasks[0].status_code` are both read, in both formats.
fn is_busy(codes: &StatusCodes) -> bool {
    let task_code = codes
        .tasks
        .as_ref()
        .and_then(|tasks| tasks.first())
        .and_then(|task| task.status_code);
    [codes.status_code, task_code]
        .into_iter()
        .flatten()
        .any(|code| BUSY_STATUS_CODES.contains(&code))
}

/// Whether a task_get response has a top-level `status_code` of 50000 or
/// higher, which counts as a failed request (see `FIRST_SERVER_ERROR`). The
/// top-level code is read in both formats; a standard-format task that itself
/// ended in such an error is a result and is not matched.
fn is_server_error(codes: &StatusCodes) -> bool {
    codes
        .status_code
        .is_some_and(|code| code >= FIRST_SERVER_ERROR)
}

/// Whether the error message of a failed request reports a timeout, such as
/// the `TimeoutError` of a result request that ran past its limit.
fn mentions_timeout(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("timeout") || lower.contains("timed out")
}

/// The error for a request that failed before its whole response arrived:
/// `504` `dataforseo_timeout` for a timeout, otherwise `502` `dataforseo_error`
/// with the HTTP status received so far (0 if none).
fn request_failed(status: u16, message: String) -> Response {
    if mentions_timeout(&message) {
        json_error(504, &message, "dataforseo_timeout").unwrap()
    } else {
        dataforseo_error(status, &message, None).unwrap()
    }
}

/// Sends one request to DataForSEO and returns its status and body text.
///
/// With `timeout_ms`, the request is abandoned if the whole response, body
/// included, has not arrived in that time; it then fails as a timeout (`504`),
/// whether the headers or the body were late.
async fn send(
    url: &str,
    method: Method,
    auth_header: &str,
    body: Option<String>,
    timeout_ms: Option<u32>,
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

    // The signal also covers reading the body, so it stays alive until then.
    let signal = timeout_ms.map(|ms| AbortSignal::from(web_sys::AbortSignal::timeout_with_u32(ms)));
    let fetch = Fetch::Request(outbound);
    let sent = match &signal {
        Some(signal) => fetch.send_with_signal(signal).await,
        None => fetch.send().await,
    };
    let mut response = sent.map_err(|e| request_failed(0, format!("Fetch failed: {e}")))?;

    let status = response.status_code();
    let body_text = response
        .text()
        .await
        .map_err(|e| request_failed(status, format!("Failed to read response: {e}")))?;
    Ok((status, body_text))
}

/// Asks DataForSEO for the task's result once.
///
/// `Ok(Some(body))` is the finished task's response, `Ok(None)` means the task
/// is still running or DataForSEO's rate limit turned the request away, and
/// `Err` is a failed request, which the caller may retry: a failed fetch or
/// timeout, a non-2xx status, a body without DataForSEO's status codes, or a
/// DataForSEO server error.
async fn poll_once(
    url: &str,
    auth_header: &str,
    ai_optimized: bool,
) -> std::result::Result<Option<String>, Response> {
    let (status, body_text) =
        send(url, Method::Get, auth_header, None, Some(POLL_TIMEOUT_MS)).await?;
    if !(200..300).contains(&status) {
        return Err(dataforseo_error(status, &body_text, None).unwrap());
    }
    let codes: StatusCodes = serde_json::from_str(&body_text)
        .map_err(|_| dataforseo_error(status, &body_text, None).unwrap())?;
    if is_server_error(&codes) {
        return Err(dataforseo_error(status, &body_text, None).unwrap());
    }
    let not_ready = is_busy(&codes) || is_pending(&codes, ai_optimized);
    Ok((!not_ready).then_some(body_text))
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
/// `status_code` the same way. A task_post that DataForSEO's rate limits turn
/// away is first sent again for up to `POST_RETRY_WINDOW_MS`.
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
    let (status, post_text, post_body) = loop {
        let (status, post_text) = send(
            TASK_POST_URL,
            Method::Post,
            &auth_header,
            Some(payload.clone()),
            None,
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(dataforseo_error(status, &post_text, None).unwrap());
        }
        let post_body: serde_json::Value = serde_json::from_str(&post_text)
            .map_err(|_| dataforseo_error(status, &post_text, None).unwrap())?;

        let elapsed_ms = Date::now().as_millis().saturating_sub(started_ms);
        let send_again =
            post_is_busy(&post_body) && elapsed_ms + POLL_INTERVAL_MS <= POST_RETRY_WINDOW_MS;
        if !send_again {
            break (status, post_text, post_body);
        }
        Delay::from(Duration::from_millis(POLL_INTERVAL_MS)).await;
    };

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
    fn only_a_rate_limited_task_post_is_sent_again() {
        for code in [40202, 40209] {
            let top_level = serde_json::json!({"status_code": code, "tasks": null});
            assert!(post_is_busy(&top_level));
            let in_task = serde_json::json!({
                "status_code": 20000,
                "tasks": [{"id": TASK_ID, "status_code": code, "status_message": "busy"}],
            });
            assert!(post_is_busy(&in_task));
        }

        let created = serde_json::json!({
            "status_code": 20000,
            "tasks": [{"id": TASK_ID, "status_code": 20100, "status_message": "Task Created."}],
        });
        assert!(!post_is_busy(&created));
        let invalid_field = serde_json::json!({
            "status_code": 20000,
            "tasks": [{"id": TASK_ID, "status_code": 40501, "status_message": "Invalid Field"}],
        });
        assert!(!post_is_busy(&invalid_field));
        let no_funds = serde_json::json!({"status_code": 40210, "tasks": null});
        assert!(!post_is_busy(&no_funds));
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

    fn codes(body: serde_json::Value) -> StatusCodes {
        serde_json::from_value(body).expect("body must have DataForSEO's status codes")
    }

    #[test]
    fn pending_is_read_from_the_right_field_for_each_format() {
        for code in [40601, 40602] {
            let standard =
                codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": code}]}));
            assert!(is_pending(&standard, false));
            let ai = codes(serde_json::json!({"id": TASK_ID, "status_code": code, "items": []}));
            assert!(is_pending(&ai, true));
        }

        let done =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 20000}]}));
        assert!(!is_pending(&done, false));
        let done_ai = codes(serde_json::json!({"id": TASK_ID, "status_code": 20000, "items": []}));
        assert!(!is_pending(&done_ai, true));

        let failed =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 40102}]}));
        assert!(!is_pending(&failed, false));
        let no_tasks = codes(serde_json::json!({"status_code": 40401, "tasks": null}));
        assert!(!is_pending(&no_tasks, false));
    }

    #[test]
    fn only_a_top_level_server_error_counts_as_a_failed_request() {
        let standard = codes(
            serde_json::json!({"status_code": 50000, "status_message": "Internal Error.", "tasks": null}),
        );
        assert!(is_server_error(&standard));
        let ai = codes(
            serde_json::json!({"status_code": 50301, "status_message": "Service Unavailable."}),
        );
        assert!(is_server_error(&ai));

        // In the standard format, a task that itself ended in an error is a
        // result: it is passed through.
        let task_error =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 50000}]}));
        assert!(!is_server_error(&task_error));
        let pending =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 40602}]}));
        assert!(!is_server_error(&pending));
    }

    #[test]
    fn a_rate_limit_is_busy_at_the_top_level_or_in_the_task() {
        for code in [40202, 40209] {
            let top_level = codes(serde_json::json!({"status_code": code, "tasks": null}));
            assert!(is_busy(&top_level));
            assert!(!is_server_error(&top_level));
            let in_task =
                codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": code}]}));
            assert!(is_busy(&in_task));
            let ai = codes(serde_json::json!({"status_code": code, "status_message": "busy"}));
            assert!(is_busy(&ai));
        }

        let done =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 20000}]}));
        assert!(!is_busy(&done));
        let task_error =
            codes(serde_json::json!({"status_code": 20000, "tasks": [{"status_code": 40102}]}));
        assert!(!is_busy(&task_error));
        let pending_ai = codes(serde_json::json!({"id": TASK_ID, "status_code": 40602}));
        assert!(!is_busy(&pending_ai));
    }

    #[test]
    fn timeouts_are_recognized_whether_the_headers_or_the_body_were_late() {
        // workerd's message when AbortSignal.timeout fires, as worker 0.9 formats it.
        let aborted = "TimeoutError: The operation was aborted due to timeout";
        assert!(mentions_timeout(&format!("Fetch failed: {aborted}")));
        assert!(mentions_timeout(&format!(
            "Failed to read response: {aborted}"
        )));
        assert!(mentions_timeout("Fetch failed: connection timed out"));
        assert!(!mentions_timeout("Fetch failed: Network connection lost."));
    }

    #[test]
    fn status_codes_are_read_from_a_full_result_and_not_from_other_bodies() {
        let result = r#"{"version":"0.1.20260101","status_code":20000,"status_message":"Ok.",
            "tasks":[{"id":"11260820-1535-0139-0000-7b3c1d2e4f5a","status_code":20000,"cost":0.0024,
            "data":{"priority":2},"result":[{"keyword":"x","items":[{"type":"organic","rank_group":1,
            "url":"https://example.com/","checks":null,"links":[{"url":"https://example.com/a"}]}]}]}]}"#;
        let parsed: StatusCodes = serde_json::from_str(result).expect("a full result must parse");
        assert_eq!(parsed.status_code, Some(20000));
        assert!(!is_pending(&parsed, false));
        assert!(!is_busy(&parsed));
        assert!(!is_server_error(&parsed));

        assert!(serde_json::from_str::<StatusCodes>("error code: 524").is_err());
        assert!(serde_json::from_str::<StatusCodes>("").is_err());
    }
}
