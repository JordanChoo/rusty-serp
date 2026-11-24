use crate::errors::{dataforseo_error, json_error};
use crate::validation::{Location, SerpRequest};
use base64::Engine;
use worker::*;

const DATAFORSEO_BASE_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/live/advanced";
const DATAFORSEO_AI_URL: &str =
    "https://api.dataforseo.com/v3/serp/google/organic/live/advanced.ai";

/// Builds the DataForSEO request body: an array holding one task.
///
/// Three flags are always on. `SerpRequest` has no field for any of them, so
/// callers cannot turn them off:
/// - `load_async_ai_overview` fetches AI Overviews that Google loads late.
/// - `resolve_goto_urls` makes DataForSEO replace Google redirect links in the
///   results with the final destination URLs (in testing, both `url` fields and
///   links inside `markdown` text). Requests get slower: DataForSEO can take up
///   to 260 seconds in total.
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

pub async fn fetch_serp(
    request: &SerpRequest,
    login: &str,
    password: &str,
) -> std::result::Result<Response, Response> {
    let url = if request.ai_optimized {
        DATAFORSEO_AI_URL
    } else {
        DATAFORSEO_BASE_URL
    };

    let payload = build_payload(request);

    let credentials = format!("{}:{}", login, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    let auth_header = format!("Basic {}", encoded);

    let headers = Headers::new();
    headers.set("Authorization", &auth_header).map_err(|_| {
        json_error(500, "Failed to build request headers", "missing_config").unwrap()
    })?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|_| {
            json_error(500, "Failed to build request headers", "missing_config").unwrap()
        })?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(payload.to_string().into()));

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

    if !(200..300).contains(&status) {
        return Err(dataforseo_error(status, &body_text, None).unwrap());
    }

    let mut resp = Response::ok(body_text)
        .map_err(|_| dataforseo_error(status, "Failed to construct response", None).unwrap())?;
    resp.headers_mut()
        .set("Content-Type", "application/json")
        .map_err(|_| dataforseo_error(status, "Failed to set response headers", None).unwrap())?;
    Ok(resp)
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
                assert_eq!(tasks[0]["resolve_goto_urls"], true);
                assert_eq!(tasks[0]["load_async_ai_overview"], true);
                assert_eq!(tasks[0]["resolve_poisoned_results"], true);
            }
        }
    }
}
