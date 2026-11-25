# rusty-serp: Product Requirements Document

## 1. Overview

### 1.1 Purpose

rusty-serp is a Rust-based Cloudflare Worker that serves as a stateless HTTP proxy to the DataForSEO SERP Google Organic Advanced API. It provides a unified, authenticated endpoint that Graph Agents (LangGraph, LangChain, Deep Agents, and similar orchestration frameworks) call as a tool to retrieve live Google SERP data.

### 1.2 Problem Statement

Graph Agents operating across multiple orchestration frameworks need a consistent, low-latency, globally distributed interface to fetch live SERP data. Directly embedding DataForSEO credentials and API logic into each agent creates credential sprawl, inconsistent error handling, and duplicated integration code. A shared Worker proxy centralizes authentication, credential management, request construction, and provides a single integration surface.

### 1.3 Design Philosophy

Adapted from the architecture patterns established in [rusty-gateway](https://github.com/JordanChoo/rusty-gateway):

- **Pure Rust**: No TypeScript shell, no WASM bridge. Compiled directly to `wasm32-unknown-unknown` via `worker-build`.
- **Minimal dependencies**: Only the `worker` crate, `serde`/`serde_json` for JSON, `console_error_panic_hook` for debugging, and `base64` for DataForSEO auth encoding.
- **Error-as-value**: Functions return `Result<T, Response>` where the `Err` variant is an already-constructed HTTP error response. No separate error type hierarchy.
- **Fail-early chain**: Validation proceeds in strict order (method check -> csvkey auth -> body parse -> field validation -> secrets -> API call) and short-circuits on first failure.
- **Single responsibility modules**: Each `.rs` file owns one concern. No cross-dependencies between peer modules.
- **Pass-through responses**: DataForSEO responses are returned verbatim to the caller. The Worker does not transform, filter, or restructure the response payload.

### 1.4 Scope

**In scope:**
- Accept SERP query parameters via POST JSON body
- Authenticate callers via `csvkey` query parameter
- Set one high-priority DataForSEO task per request (`task_post`) and poll for its result (`task_get/advanced`) within the same request (Section 4.6 explains why)
- Fetch the result in the standard or AI-optimized (`.ai`) format based on the `ai_optimized` flag
- Always set `load_async_ai_overview: true` in outbound requests
- Always set `resolve_goto_urls: true` in outbound requests
- Always set `resolve_poisoned_results: true` in outbound requests
- Return DataForSEO responses verbatim
- Health check endpoint

**Out of scope:**
- Response caching (no KV, no Cache API)
- Response transformation or filtering
- Rate limiting (rely on DataForSEO's built-in limits)
- Batches of tasks, normal-priority tasks, and asynchronous delivery to the caller (pingbacks, postbacks, or a task ID to collect later): every request is answered with its own result
- Non-Google search engines
- Non-organic SERP types (maps, news, images as primary)

---

## 2. Architecture

### 2.1 System Context

```
+------------------+       POST /v1/serp?csvkey=xxx        +-------------+
|                  | ------------------------------------> |             |
|   Graph Agent    |                                       | rusty-serp  |
|  (LangGraph /    | <------------------------------------ | (CF Worker) |
|   LangChain /    |       JSON response (pass-through)    |             |
|   Deep Agents)   |                                       +------+------+
|                  |                                              |
+------------------+                                              |
                                                                  | 1. POST task_post
                                                                  | 2. GET task_get every 6 s
                                                                  |    until the task is done
                                                                  | (Basic Auth)
                                                                  v
                                                    +----------------------------+
                                                    |        DataForSEO          |
                                                    | /v3/serp/google/organic/   |
                                                    |   task_post                |
                                                    |   task_get/advanced[.ai]   |
                                                    +----------------------------+
```

### 2.2 Request Flow

```
1. Agent sends POST to https://rusty-serp.<domain>/v1/serp?csvkey=<key>
2. Worker routes by method + path:
   - GET or HEAD on /v1/health -> return health response (HEAD returns no body)
   - POST on /v1/serp -> continue to step 3
   - Known path, wrong method -> 405
   - Unknown path -> 404
3. Worker extracts `csvkey` from query string (reject missing with 400)
4. Worker reads CSVKEY secret from environment (reject missing with 500)
5. Worker performs constant-time comparison (reject mismatch with 401)
6. Worker reads request body as text (reject unreadable/non-UTF-8 with 400)
7. Worker parses body as JSON (reject invalid JSON with 400)
8. Worker validates all fields against schema (reject invalid with 400):
   - keyword: required, string, non-empty, <= 700 chars
   - location: required, integer (location_code) or string (location_name)
   - depth: optional, integer 1-200, default 10
   - device: optional, "desktop" or "mobile", default "desktop"
   - language: optional, non-empty string, default "en"
   - ai_optimized: optional, must be boolean if present, default false
9. Worker reads DATAFORSEO_LOGIN and DATAFORSEO_PASSWORD from environment (reject missing with 500)
10. Worker constructs the task_post body (priority: 2; always sets load_async_ai_overview: true, resolve_goto_urls: true and resolve_poisoned_results: true)
11. Worker sends POST to .../task_post with Basic Auth
    - non-2xx or unreadable response -> 502
    - task not created (tasks[0].status_code != 20100) -> return the task_post response verbatim with status 200
12. Worker waits 10 s, then sends GET with Basic Auth to the result URL:
    - ai_optimized == true  -> .../task_get/advanced/{id}.ai
    - ai_optimized == false -> .../task_get/advanced/{id}
13. Task status 40601 (Task Handed) or 40602 (Task In Queue) -> wait 6 s and repeat step 12
    - a top-level 40202 or 40209 (DataForSEO's rate limits) is treated the same way
    - a failed result request (no full answer within 10 s, non-2xx, not JSON, or a status_code of 50000 or higher) is retried the same way; the third failure in a row -> 502 (504 if it timed out)
    - no answer within 280 s of step 11 -> 504 dataforseo_timeout with dataforseo_task_id
14. Worker returns the finished task's response body verbatim with status 200
```

### 2.3 Module Structure

```
rusty-serp/
  Cargo.toml
  Cargo.lock
  wrangler.toml
  LICENSE
  README.md
  prd/
    rusty-serp-prd.md        # This document
  src/
    lib.rs                   # Entry point, routing, orchestration
    validation.rs            # Request body parsing, field validation, csvkey auth
    dataforseo.rs            # DataForSEO API client (task_post, polling task_get, response handling)
    errors.rs                # JSON error response builders
```

**Module responsibilities:**

| Module | Responsibility | Imports from |
|--------|---------------|-------------|
| `lib.rs` | `#[event(fetch)]` entry point, HTTP method check, orchestrates validation -> dataforseo call chain | `validation`, `dataforseo`, `errors` |
| `validation.rs` | Parses `csvkey` from query params, reads and validates JSON body, performs constant-time auth comparison, validates all input fields | `errors` |
| `dataforseo.rs` | Constructs the task_post body (`priority: 2`, always setting `load_async_ai_overview`, `resolve_goto_urls` and `resolve_poisoned_results` to `true`), builds Basic Auth header, sets the task, polls `task_get/advanced` (or `.ai`) until the task is done or the deadline passes, handles HTTP-level errors | `errors` |
| `errors.rs` | Provides `json_error()`, `dataforseo_error()` and `task_timeout_error()` response builder functions, UTF-8-safe body truncation | (none) |

---

## 3. API Specification

### 3.1 Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/v1/health` | No | Liveness probe. Returns `{"status": "ok"}` with 200. |
| `POST` | `/v1/serp` | Yes (`csvkey` query param) | Execute a live SERP query against DataForSEO. |

All other methods and paths return 404.

### 3.2 Authentication

Authentication uses the `csvkey` query parameter, consistent with the pattern established in rusty-gateway and edgentities.

**Flow:**
1. Extract `csvkey` from the request URL query string.
2. If absent, return `400` with code `missing_csvkey`.
3. Read the `CSVKEY` secret from the Cloudflare Worker environment.
4. If the secret is not configured, return `500` with code `missing_config`.
5. Perform constant-time byte comparison between the provided key and the stored secret.
6. If mismatch, return `401` with code `unauthorized`.

**Constant-time comparison** (adapted from rusty-gateway):

```rust
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
```

The length check is an accepted trade-off: it leaks key length but prevents timing attacks on key content.

### 3.3 Request Schema (`POST /v1/serp`)

The request body is a JSON object with the following fields:

| Field | Type | Required | Default | Constraints | Description |
|-------|------|----------|---------|-------------|-------------|
| `keyword` | string | **Yes** | -- | 1-700 characters. Must not be empty or whitespace-only. `%` must be encoded as `%25`, `+` as `%2B` in the keyword value if literal characters are intended. | The Google search query to execute. Supports Google search operators (`site:`, `intitle:`, etc.) but note that operators multiply DataForSEO cost by 5x. |
| `location` | string or integer | **Yes** | -- | String: valid location name (e.g., `"United States"`, `"London,England,United Kingdom"`). Integer: valid DataForSEO location code (e.g., `2840` for US, `2826` for UK). | The geographic location for the search. The Worker auto-detects whether the value is a name (string) or code (integer) and maps to `location_name` or `location_code` in the DataForSEO request. |
| `depth` | integer | No | `10` | Range: 1-200 (DataForSEO's documented maximum). Charged per 10 results by DataForSEO. | Number of SERP results to retrieve. |
| `device` | string | No | `"desktop"` | Enum: `"desktop"`, `"mobile"`. Case-insensitive (normalized to lowercase). | Device type to emulate for the search. |
| `language` | string | No | `"en"` | Valid DataForSEO language code (e.g., `"en"`, `"es"`, `"fr"`, `"de"`, `"ja"`). | Language code for the search results. Mapped to `language_code` in the DataForSEO request. |
| `ai_optimized` | boolean | No | `false` | Must be a JSON boolean (`true` or `false`). Strings like `"true"`, integers like `1`, and `null` are **not** accepted — they return `400` with code `invalid_ai_optimized`. | When `true`, the Worker calls the `.ai` suffixed endpoint (`/live/advanced.ai`) which returns a flattened, token-efficient response format with null/false/empty fields stripped. When `false`, calls the standard endpoint (`/live/advanced`). |

**Authentication parameter (query string, not body):**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `csvkey` | string | **Yes** | API key passed as a URL query parameter. Validated against the `CSVKEY` environment secret. |

#### 3.3.1 Location Field Behavior

The `location` field accepts either a string or integer, and the Worker maps it to the correct DataForSEO field:

| Input Type | Example | Maps To | DataForSEO Field |
|------------|---------|---------|-----------------|
| Integer | `2840` | Location code | `location_code: 2840` |
| String | `"United States"` | Location name | `location_name: "United States"` |
| String | `"London,England,United Kingdom"` | Location name | `location_name: "London,England,United Kingdom"` |

**Detection logic:** Type detection is based strictly on the JSON value type, **not** on string content:

- If the JSON value is a **number** (`serde_json::Value::Number`), treat it as a location code. It must be a positive integer.
- If the JSON value is a **string** (`serde_json::Value::String`), treat it as a location name. It must not be empty after trimming.
- A string like `"2840"` is treated as a **location name**, not a location code. The Worker does **not** attempt to parse strings as integers. Agents that want to use a location code must send it as a JSON number (`2840`), not a JSON string (`"2840"`).
- Any other JSON type (boolean, null, array, object) returns `400` with code `missing_location`.

#### 3.3.2 Example Request

```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "keyword": "best project management tools 2026",
    "location": 2840,
    "depth": 20,
    "device": "desktop",
    "language": "en",
    "ai_optimized": true
  }'
```

#### 3.3.3 Example Request (Location Name)

```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "keyword": "best coffee shops near me",
    "location": "London,England,United Kingdom",
    "depth": 10,
    "device": "mobile",
    "language": "en",
    "ai_optimized": false
  }'
```

#### 3.3.4 Minimal Request (Defaults Applied)

```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "keyword": "rust programming language",
    "location": 2840
  }'
```

This applies defaults: `depth: 10`, `device: "desktop"`, `language: "en"`, `ai_optimized: false`.

### 3.4 Response Schema

#### 3.4.1 Success Response (Pass-Through)

The Worker returns the DataForSEO response body verbatim: the `task_get/advanced` response for the finished task (Section 4.1). The response shape depends on the `ai_optimized` flag. In the standard format, `time` is how long the result request took (not the search), `path` names `task_get` and ends with the task ID, `data` echoes the task_post parameters including `priority`, and `cost` is the task's full cost.

**When `ai_optimized: false` (standard response):**

```json
{
  "version": "0.1.20261117",
  "status_code": 20000,
  "status_message": "Ok.",
  "time": "0.0314 sec.",
  "cost": 0.0042,
  "tasks_count": 1,
  "tasks_error": 0,
  "tasks": [
    {
      "id": "05281810-1535-0121-0000-014aadae6b3a",
      "status_code": 20000,
      "status_message": "Ok.",
      "time": "0.0276 sec.",
      "cost": 0.0042,
      "result_count": 1,
      "path": ["v3", "serp", "google", "organic", "task_get", "advanced", "05281810-1535-0121-0000-014aadae6b3a"],
      "data": {
        "api": "serp",
        "function": "task_get",
        "se": "google",
        "se_type": "organic",
        "keyword": "best project management tools 2026",
        "location_code": 2840,
        "language_code": "en",
        "device": "desktop",
        "os": "windows",
        "depth": 20,
        "priority": 2,
        "load_async_ai_overview": true,
        "resolve_goto_urls": true,
        "resolve_poisoned_results": true
      },
      "result": [
        {
          "keyword": "best project management tools 2026",
          "type": "organic",
          "se_domain": "google.com",
          "location_code": 2840,
          "language_code": "en",
          "check_url": "https://www.google.com/search?q=best%20project%20management%20tools%202026&num=20&hl=en&gl=US&uule=...",
          "datetime": "2026-05-22 14:30:00 +00:00",
          "spell": null,
          "refinement_chips": null,
          "item_types": ["organic", "people_also_ask", "related_searches", "ai_overview"],
          "se_results_count": 1240000000,
          "pages_count": 1,
          "items_count": 20,
          "items": [
            {
              "type": "ai_overview",
              "rank_group": 1,
              "rank_absolute": 1,
              "position": "left",
              "xpath": "/html/body/...",
              "asynchronous_ai_overview": true,
              "items": ["...ai_overview_element objects..."]
            },
            {
              "type": "organic",
              "rank_group": 1,
              "rank_absolute": 2,
              "position": "left",
              "xpath": "/html/body/...",
              "domain": "example.com",
              "title": "Best Project Management Tools in 2026",
              "url": "https://example.com/best-pm-tools",
              "description": "...",
              "breadcrumb": "https://example.com > tools > pm",
              "is_image": false,
              "is_video": false,
              "is_featured_snippet": false,
              "is_malicious": false,
              "is_web_story": false,
              "highlighted": ["project management tools"],
              "links": null,
              "rating": null,
              "price": null,
              "about_this_result": null,
              "related_result": null,
              "timestamp": "2026-05-20 00:00:00 +00:00",
              "rectangle": null
            }
          ]
        }
      ]
    }
  ]
}
```

**When `ai_optimized: true` (AI-optimized response):**

```json
{
  "id": "05281810-1535-0121-0000-014aadae6b3a",
  "status_code": 20000,
  "status_message": "Ok.",
  "items": [
    {
      "type": "ai_overview",
      "rank_group": 1,
      "rank_absolute": 1,
      "asynchronous_ai_overview": true,
      "items": ["...ai_overview_element objects..."]
    },
    {
      "type": "organic",
      "rank_group": 1,
      "rank_absolute": 2,
      "domain": "example.com",
      "title": "Best Project Management Tools in 2026",
      "url": "https://example.com/best-pm-tools",
      "description": "...",
      "breadcrumb": "https://example.com > tools > pm",
      "highlighted": ["project management tools"],
      "timestamp": "2026-05-20 00:00:00 +00:00"
    }
  ]
}
```

Note: The AI-optimized format strips `null`/`false`/empty fields, removes `position` and `xpath`, flattens the envelope, and rounds floats to 3 decimal places. This is handled entirely by DataForSEO's `.ai` format of `task_get/advanced` — the Worker passes it through unchanged. It has the same fields as the `.ai` format of the Live endpoint.

**Task not created:** If DataForSEO does not create the task (for example, an invalid field or an empty balance), the Worker returns the `task_post` response instead, verbatim, with the error in `tasks[0].status_code`. That response uses the standard envelope even when `ai_optimized` is `true`, because `task_post` has no `.ai` format.

**HTTP Status:** The Worker returns `200` for all successful DataForSEO responses, regardless of the DataForSEO task-level `status_code`. The caller inspects the `status_code` field in the JSON body to determine DataForSEO-level success or failure.

**Content-Type:** `application/json`

#### 3.4.2 Error Responses

All error responses follow a consistent JSON schema:

```json
{
  "error": "Human-readable error message",
  "code": "machine_readable_error_code"
}
```

**Content-Type:** `application/json`

#### 3.4.3 Error Code Taxonomy

| HTTP Status | Code | Condition |
|-------------|------|-----------|
| 400 | `missing_csvkey` | `csvkey` query parameter not present in URL |
| 400 | `missing_body` | Request body is empty, not readable, or not valid UTF-8 |
| 400 | `invalid_json` | Request body is not valid JSON |
| 400 | `missing_keyword` | `keyword` field is absent or empty |
| 400 | `invalid_keyword` | `keyword` exceeds 700 characters |
| 400 | `missing_location` | `location` field is absent |
| 400 | `invalid_location` | `location` is neither a valid string nor integer |
| 400 | `invalid_depth` | `depth` is not an integer in range 1-200 |
| 400 | `invalid_device` | `device` is not `"desktop"` or `"mobile"` |
| 400 | `invalid_language` | `language` is empty or not a string |
| 400 | `invalid_ai_optimized` | `ai_optimized` is present but is not a JSON boolean (`true`/`false`) |
| 401 | `unauthorized` | `csvkey` does not match the `CSVKEY` secret |
| 404 | `not_found` | Unrecognized path |
| 405 | `method_not_allowed` | Non-POST method on `/v1/serp`, non-GET/HEAD on `/v1/health` |
| 500 | `missing_config` | `CSVKEY`, `DATAFORSEO_LOGIN`, or `DATAFORSEO_PASSWORD` secret is not configured |
| 502 | `dataforseo_error` | DataForSEO returned an HTTP error (non-2xx status) or a body that is not JSON, **or** the outbound fetch to DataForSEO failed entirely (network error, DNS failure, TLS error). For the task_post request this happens on the first failure; result requests are retried, and the third failure in a row is returned. For result requests, a DataForSEO server error (HTTP 200 with a top-level `status_code` of 50000 or higher) also counts as a failure, because it says nothing about the task. When the fetch never reached DataForSEO, the response will have `"dataforseo_status": 0` — this tells the caller the failure was at the network level, not a DataForSEO HTTP error. |
| 504 | `dataforseo_timeout` | The task did not finish within 280 seconds of being set (or DataForSEO's rate limits turned result requests away until then). The body adds `"dataforseo_task_id"`; DataForSEO may still finish the task and charge for it. The same code, without the task ID, is returned when an outbound request, or reading its response, fails with an error whose message contains "timeout" or "timed out", including when three result requests in a row fail and the last one did not answer in full within 10 seconds. That should be rare. If the caller's own timeout fires first, the caller gets no response at all (see Section 4.6). |

#### 3.4.4 DataForSEO Error Response

When DataForSEO returns a non-2xx HTTP status, the Worker returns a `502` with additional context:

```json
{
  "error": "DataForSEO request failed",
  "code": "dataforseo_error",
  "dataforseo_status": 401,
  "dataforseo_body": "... (truncated to 4KB, UTF-8 safe) ..."
}
```

This mirrors the `brightdata_error` pattern from rusty-gateway: always 502, includes the upstream status code and a truncated body for debugging, and performs UTF-8-safe truncation at character boundaries.

**When the fetch fails before reaching DataForSEO** (network error, DNS failure, etc.), the Worker returns:

```json
{
  "error": "DataForSEO request failed",
  "code": "dataforseo_error",
  "dataforseo_status": 0,
  "dataforseo_body": "Fetch failed: NetworkError when attempting to fetch resource."
}
```

A `dataforseo_status` of `0` means the request never reached DataForSEO — it is not a real HTTP status code. Agents should treat this as a transient infrastructure error and retry.

---

## 4. DataForSEO Integration

### 4.1 Endpoint Selection

The Worker uses DataForSEO's Standard method: it sets a task and then fetches the task's result. Section 4.6 explains why it does not use the Live endpoint.

| Step | `ai_optimized` Value | DataForSEO Endpoint |
|------|----------------------|-------------------|
| Set the task | either | `POST https://api.dataforseo.com/v3/serp/google/organic/task_post` |
| Fetch the result | `false` (default) | `GET https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced/{id}` |
| Fetch the result | `true` | `GET https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced/{id}.ai` |

`{id}` is the task ID from the task_post response (`tasks[0].id`, given when `tasks[0].status_code` is `20100`, "Task Created."). The Worker only puts it in the URL if it is a 36-character UUID.

### 4.2 Authentication

DataForSEO uses HTTP Basic Authentication. The Worker constructs the `Authorization` header from two secrets:

```
Authorization: Basic base64(DATAFORSEO_LOGIN:DATAFORSEO_PASSWORD)
```

The base64 encoding is performed at request time using the `base64` crate. Credentials are never logged, cached, or included in error responses.

### 4.3 Request Construction

The Worker constructs the task_post body as a JSON array containing a single task object. The following fields are always set:

| DataForSEO Field | Source | Notes |
|-----------------|--------|-------|
| `keyword` | `request.keyword` | Passed through as-is |
| `location_code` | `request.location` (when integer) | Set when location is numeric |
| `location_name` | `request.location` (when string) | Set when location is a string |
| `depth` | `request.depth` (default: `10`) | Passed through |
| `device` | `request.device` (default: `"desktop"`) | Normalized to lowercase |
| `language_code` | `request.language` (default: `"en"`) | Passed through |
| `priority` | **Always `2`** | High execution priority. Normal-priority tasks wait in DataForSEO's queue and can take minutes; high-priority tasks start at once and cost more per page (Section 4.7). |
| `load_async_ai_overview` | **Always `true`** | Hardcoded. Never overridden by caller input. |
| `resolve_goto_urls` | **Always `true`** | Hardcoded. Never overridden by caller input. It must be sent: for high-priority tasks it defaults to `false`. DataForSEO replaces Google redirect ("goto") links in the results with final destination URLs and returns unresolvable links unchanged. In testing, that covered both `url` fields and links inside `markdown` text. DataForSEO says it adds up to 40 seconds to a high-priority task, for a turnaround of at most 260 seconds. |
| `resolve_poisoned_results` | **Always `true`** | Hardcoded. Never overridden by caller input. DataForSEO checks the first results page against Google's Web view. If the page looks poisoned (organic listings swapped for unrelated ones), it replaces the affected organic results with the verified ones, appends any extra Web-view listings, and collects any further pages (`depth` above 10) from the Web view as well. Each replaced or added result has `from_web` in its `checks` array, in both response formats. Other SERP features on the first page are returned as collected. Every task is charged for one extra SERP page and waits for one extra Google request. Cannot be combined with `udm=web`/`udm=14`, which the Worker never sends. In testing (`"best running shoes"`, US), 9 of the 10 organic results came back marked `from_web`. |

**Example outbound request body:**

```json
[
  {
    "keyword": "best project management tools 2026",
    "location_code": 2840,
    "depth": 20,
    "device": "desktop",
    "language_code": "en",
    "priority": 2,
    "load_async_ai_overview": true,
    "resolve_goto_urls": true,
    "resolve_poisoned_results": true
  }
]
```

### 4.4 Outbound Request Headers

| Header | Value |
|--------|-------|
| `Authorization` | `Basic <base64(login:password)>` (every request) |
| `Content-Type` | `application/json` (task_post only; the task_get requests have no body) |

### 4.5 Response Handling

**task_post:**
1. **Read the response body as text.** This is done regardless of status code to capture error details.
2. **Check HTTP status code.** If not in the 2xx range, or the body is not JSON, return a `502` `dataforseo_error` response with the upstream status and truncated body.
3. **Check that the task was created** (`tasks[0].status_code` is `20100`). If not, return the task_post response verbatim with status `200`: it carries DataForSEO's error (for example `40501` invalid field or `40210` insufficient funds) in `tasks[0].status_code`, like any other task-level error.

**task_get (polling):**
1. Wait 10 seconds after the task is set, then request the result.
2. A failed fetch, no full answer within 10 seconds, a non-2xx status, a body that is not JSON, or a DataForSEO server error (a top-level `status_code` of 50000 or higher, which DataForSEO sends with HTTP 200) counts as a failed request. The Worker retries it at the next interval; the third failure in a row is returned to the caller (`502` `dataforseo_error`, or `504` `dataforseo_timeout` when the last one timed out). In the `.ai` format the top-level code is also the task's own, so a task that itself ended with a code of 50000 or higher also ends as a `502` after three tries.
3. Read the task's status code: `tasks[0].status_code` in the standard format, the top-level `status_code` in the `.ai` format. `40601` (Task Handed) and `40602` (Task In Queue) mean the task is still running: wait 6 seconds and ask again. A top-level `40202` or `40209` (DataForSEO's rate limits per minute and for simultaneous requests, sent with HTTP 200) says nothing about the task and is treated the same way.
4. Any other status code, or none, means the task is finished: return the response body verbatim with status `200` and `Content-Type: application/json`. That includes task-level errors such as `40102` (no search results).
5. If 280 seconds have passed since the task was set, return `504` `dataforseo_timeout` with `dataforseo_task_id` instead of asking again.

The Worker reads only the task ID and status codes. It does not validate or transform the rest of the response; the body is passed through to the caller as received. A result's status codes are read without building the rest of the document, which keeps CPU time low: for a 1.9 MB result (about depth 200), a native benchmark took 1.3 ms this way against 13 ms to parse the whole document, and Workers Free allows 10 ms of CPU time per request.

### 4.6 Timeout Handling

**Why the Worker does not use the Live endpoint.** Cloudflare ends a Worker's outbound request with a `524` error when the server sends nothing for 125 seconds. This is the Proxy Read Timeout in Cloudflare's [connection limits](https://developers.cloudflare.com/fundamentals/reference/connection-limits/); only Enterprise zones can raise it, and a workers.dev Worker cannot. Cloudflare's Workers limits page says "There is no set time limit on individual subrequests", but that does not hold for a fetch to an external server: in production on 2026-11-25, both Live requests (`"best running shoes"`, US, depth 10) failed after 125.2 seconds with `"dataforseo_status": 524`. With redirect resolution and the poisoned-result check always on, Live requests took 105 to 140 seconds in testing, so many would fail this way. (Under `wrangler dev` the same requests succeed, because they don't pass through Cloudflare's network.)

**How the Worker avoids it.** Every outbound request is short: task_post returns at once, and so does each task_get. Only the Worker's answer to its own caller takes long, and Cloudflare does not cut that off while the caller stays connected: a test Worker on workers.dev answered after 150 and 200 seconds. Time spent waiting on DataForSEO or on a timer does not count toward the Worker's CPU time limit, so this works on any Workers plan.

**Timing.** DataForSEO says `resolve_goto_urls` adds up to 40 seconds to a high-priority task, for a turnaround of at most 260 seconds. `resolve_poisoned_results` adds one Google request to every task; DataForSEO gives no time figure for it. In testing (`"best running shoes"`, US, depth 10), requests took 145 to 186 seconds end to end: 145 and 163 s on `wrangler dev`, 170 and 157 s on the staging Worker, and 186 and 162 s in production on 2026-11-25.

**Limits.** The first result request goes out 10 seconds after the task is set, then one every 6 seconds. A result request that has not fully answered within 10 seconds is abandoned and counts as a failure. After 280 seconds the Worker stops and returns `504` `dataforseo_timeout` with the task ID. The task_post request has no time limit of its own, because abandoning it could leave a paid task whose ID nobody has; Cloudflare's 125-second cutoff bounds it. That is at most 47 outbound requests per call (one task_post and up to 46 task_get), within the 50 subrequests per invocation that Workers Free allows. Both limits are checked when the code compiles.

**What actually happens in each scenario:**

| Scenario | What the caller sees |
|----------|---------------------|
| The task finishes within 280 seconds | Normal `200` response with DataForSEO's JSON body, including task-level errors in its `status_code`. |
| The task is still running after 280 seconds, or DataForSEO's rate limits turn result requests away until then | A `504` JSON response with code `dataforseo_timeout` and `dataforseo_task_id`. DataForSEO may still finish the task and charge for it. |
| Three result requests in a row fail (including timeouts and DataForSEO server errors) | A `502` `dataforseo_error` (or `504` `dataforseo_timeout` if the last one timed out) describing the last failure. |
| The caller's own timeout fires first | The caller gets nothing from rusty-serp; its HTTP client reports a timeout. Cloudflare may cancel the Worker, and the result is lost, though DataForSEO charged for the task when it was set. |
| The Worker throws an exception or the connection drops | A non-JSON Cloudflare error page or a connection reset, **not** a structured JSON response. |

**Implication for agents:** Set the agent tool timeout to 300 seconds. The Worker answers by about 290 seconds at the latest (its 280-second deadline plus one result request, which it abandons after 10 seconds), so a 300-second timeout always gets the Worker's answer. Agents should handle a `200` with a task-level error in `status_code`, a `504` `dataforseo_timeout`, a `502` `dataforseo_error`, and a non-JSON error page or connection reset.

### 4.7 Cost Implications

The Standard method is charged when the task is set; fetching the result is free. High priority costs more per page than normal priority, but less than Live.

| Scenario | DataForSEO Cost |
|----------|----------------|
| Basic query, 10 results | $0.0012 (one page at high priority) + $0.0006 (`load_async_ai_overview`) + $0.0012 (`resolve_poisoned_results`) = **$0.003** |
| Basic query, 20 results | $0.0024 (2 pages) + $0.0006 + $0.0012 = **$0.0042** |
| AI overview absent from SERP | `load_async_ai_overview` surcharge refunded |
| AI overview was cached (not async) | `load_async_ai_overview` surcharge refunded |
| Query with search operators | Base cost x5 |
| `.ai` format (AI-optimized) | No additional cost |
| `resolve_goto_urls` (always on) | No extra charge listed by DataForSEO |
| `resolve_poisoned_results` (always on) | Price of one more SERP page ($0.0012 at high priority) per task, fixed regardless of depth and charged even when nothing was poisoned. |

In testing, a 10-result task cost $0.0024 with the AI overview surcharge refunded: two pages at $0.0012, one of them for the poisoned-result check. The same query on the Live endpoint cost $0.004.

---

## 5. Secrets & Configuration

### 5.1 Cloudflare Worker Secrets

These are set via `wrangler secret put <NAME>` and accessed via `env.var("<NAME>")` in the `worker` crate. They are never stored in `wrangler.toml` or committed to source control.

| Secret Name | Description | Example |
|-------------|-------------|---------|
| `CSVKEY` | Shared secret for authenticating inbound requests via the `csvkey` query parameter | `sk-rusty-serp-prod-abc123xyz` |
| `DATAFORSEO_LOGIN` | DataForSEO API login (email) | `user@example.com` |
| `DATAFORSEO_PASSWORD` | DataForSEO API password (auto-generated, from app.dataforseo.com/api-access) | `a1b2c3d4e5f6g7h8` |

### 5.2 Wrangler Configuration

```toml
name = "rusty-serp"
main = "build/worker/shim.mjs"
compatibility_date = "2026-05-03"

[build]
# worker-build accepts only a `worker` crate of its own minor version (0.9.x here),
# and patch releases have broken builds too, so it is pinned exactly. Bump it
# together with `worker` in Cargo.toml.
command = "curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable && . \"$HOME/.cargo/env\" && rustup target add wasm32-unknown-unknown && cargo install -q worker-build --version 0.9.0 --locked && worker-build --release"

[env.staging]
name = "rusty-serp-staging"

[env.production]
name = "rusty-serp"
```

**Key design decisions:**
- **Self-bootstrapping build command**: Installs Rust, the WASM target, and `worker-build` if not already present. Same pattern as rusty-gateway, except that `worker-build` is pinned.
- **Pinned `worker-build`**: `--version 0.9.0 --locked` installs an exact release. `worker-build` accepts only a `worker` crate of its own minor version, so once `worker-build` 0.9.0 came out, the unpinned install could no longer build this project on `worker` 0.8. The pin is exact rather than `^0.9` because a patch release can break the build too: `worker-build` 0.8.7 passes wasm-bindgen a flag that the CLI version locked here at the time (0.2.122) did not support. Bump it together with `worker` in `Cargo.toml`.
- **No `[vars]` section**: All configuration is via secrets. No environment variables are needed for this Worker.
- **Staging and production environments**: Allows separate secret sets for testing vs. production.
- **Top-level `name` is `rusty-serp`, the production Worker**: Production is deployed by Cloudflare Workers Builds, which is connected to the `rusty-serp` Worker and runs `npx wrangler deploy` on every push to `main`. Workers Builds fails the build unless the top-level `name` matches the connected Worker. It opened a pull request (#1, May 2026) renaming the original `rusty-serp-dev` to `rusty-serp`. As a result, a bare `wrangler deploy` from a developer machine also deploys to production, and so does `--env production`, which names the same Worker. There is no separate dev Worker. Use `--env staging` (`rusty-serp-staging`) for anything that must not reach production, and `wrangler dev` for local testing.

### 5.3 Cargo Configuration

```toml
[package]
name = "rusty-serp"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
worker = "0.9"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
base64 = "0.22"
console_error_panic_hook = "0.1"

[profile.release]
opt-level = "z"
lto = true
strip = true
```

**Dependency rationale:**

| Crate | Version | Why |
|-------|---------|-----|
| `worker` | `0.9` | Cloudflare Workers runtime bindings. Provides `Request`, `Response`, `Env`, `Fetch`, `Headers`, `Method`. Must match the minor version of the `worker-build` pinned in `wrangler.toml`. Runs every handler under the runtime's active trace span, so it needs a Workers runtime with `tracing.getActiveSpan` and `tracing.withActiveSpan` (the latter added November 2026); see Section 9.3 for what that means for `wrangler dev`. |
| `serde` | `1` (derive) | Deserialize inbound JSON request body into typed struct. Serialize outbound DataForSEO request. |
| `serde_json` | `1` | JSON parsing and construction. Used for request body deserialization and DataForSEO request body construction. |
| `base64` | `0.22` | Encode `login:password` for DataForSEO Basic Auth header. |
| `console_error_panic_hook` | `0.1` | Better panic messages in WASM/Workers environment for debugging. |

**Release profile:**
- `opt-level = "z"`: Optimize for smallest binary size (critical for WASM cold-start performance).
- `lto = true`: Link-time optimization for further size reduction.
- `strip = true`: Strip debug symbols from release binary.

---

## 6. Implementation Specification

### 6.1 `src/lib.rs` — Entry Point & Routing

**Responsibilities:**
- Register the `#[event(fetch)]` handler
- Set panic hook
- Route by method and path
- Orchestrate the validation -> DataForSEO call chain

**Routing table:**

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/v1/health` | Return `{"status": "ok"}` with 200 |
| `HEAD` | `/v1/health` | Return 200 with `Content-Type: application/json` header but **no body**. This supports monitoring tools (e.g., uptime checkers, load balancers) that use HEAD requests for liveness probes. |
| `POST` | `/v1/serp` | Execute SERP query pipeline |
| `*` | `/v1/health` | Return 405 `method_not_allowed` |
| `*` | `/v1/serp` | Return 405 `method_not_allowed` |
| `*` | `*` | Return 404 `not_found` |

**Content-Type request validation:** The Worker does **not** validate the `Content-Type` header on inbound requests to `POST /v1/serp`. If a caller sends `application/x-www-form-urlencoded` or omits `Content-Type` entirely, the Worker will still attempt to read the body as text and parse it as JSON. If the body is not valid JSON, the caller receives `400` with code `invalid_json`. This is intentional — checking `Content-Type` adds complexity without meaningful security benefit since the Worker only ever interprets the body as JSON regardless.

**Pseudocode:**

```rust
#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let path = req.path();
    let method = req.method();

    match (method, path.as_str()) {
        (Method::Get, "/v1/health") => {
            // Return {"status": "ok"} with 200
            let body = serde_json::json!({"status": "ok"});
            let mut resp = Response::ok(body.to_string())?;
            resp.headers_mut().set("Content-Type", "application/json")?;
            Ok(resp)
        }
        (Method::Head, "/v1/health") => {
            // Return 200 with Content-Type header but empty body.
            // Monitoring tools use HEAD for liveness probes.
            let mut resp = Response::empty()?.with_status(200);
            resp.headers_mut().set("Content-Type", "application/json")?;
            Ok(resp)
        }
        (Method::Post, "/v1/serp") => {
            handle_serp(req, env).await
        }
        (_, "/v1/health") | (_, "/v1/serp") => {
            json_error(405, "Method not allowed", "method_not_allowed")
        }
        _ => {
            json_error(404, "Not found", "not_found")
        }
    }
}

async fn handle_serp(mut req: Request, env: Env) -> Result<Response> {
    // 1. Auth
    let url = req.url()?;
    validate_auth(&url, &env)?;

    // 2. Parse & validate body
    let serp_request = parse_and_validate_body(&mut req).await?;

    // 3. Read DataForSEO credentials
    let login = read_secret(&env, "DATAFORSEO_LOGIN")?;
    let password = read_secret(&env, "DATAFORSEO_PASSWORD")?;

    // 4. Execute DataForSEO request
    let response = fetch_serp(&serp_request, &login, &password).await?;

    Ok(response)
}
```

### 6.2 `src/validation.rs` — Auth & Input Validation

**Structs:**

```rust
#[derive(Debug)]
pub struct SerpRequest {
    pub keyword: String,
    pub location: Location,
    pub depth: u32,
    pub device: String,
    pub language: String,
    pub ai_optimized: bool,
}

#[derive(Debug)]
pub enum Location {
    Code(i64),
    Name(String),
}
```

**Functions:**

```rust
/// Extract csvkey from query string and validate against CSVKEY secret.
/// Returns Err(Response) on missing key, missing config, or mismatch.
pub fn validate_auth(url: &Url, env: &Env) -> std::result::Result<(), Response>

/// Read request body, parse as JSON, validate all fields.
/// Returns Err(Response) on empty body, invalid JSON, or field validation failures.
pub async fn parse_and_validate_body(req: &mut Request) -> std::result::Result<SerpRequest, Response>

/// Parse and validate the body text. Split out so tests can call it without a Request.
fn validate_body(body_text: &str) -> std::result::Result<SerpRequest, Response>

/// Read a secret from the environment. Returns Err(Response) with 500 if not configured.
pub fn read_secret(env: &Env, name: &str) -> std::result::Result<String, Response>

/// Constant-time byte comparison for auth tokens.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool
```

**Validation rules:**

| Field | Validation | Error |
|-------|-----------|-------|
| `keyword` | Must be present. Must be a string. Must not be empty/whitespace after trim. Must be <= 700 chars. | `missing_keyword` or `invalid_keyword` |
| `location` | Must be present. If JSON number, treat as location code (must be positive integer). If JSON string, treat as location name (must not be empty). A string like `"2840"` is treated as a location **name**, not a code — the Worker does not parse strings as integers. Any other JSON type (boolean, null, array, object) is rejected. | `missing_location` or `invalid_location` |
| `depth` | If present, must be integer in range 1-200. | `invalid_depth` |
| `device` | If present, must be `"desktop"` or `"mobile"` (case-insensitive). | `invalid_device` |
| `language` | If present, must be a non-empty string. | `invalid_language` |
| `ai_optimized` | If present, must be a JSON boolean (`true` or `false`). Strings, integers, null, and other types are **rejected** — not silently coerced. | `invalid_ai_optimized` |

**Body parsing strategy:**

The body is read as text first, then parsed as `serde_json::Value` for flexible location type detection. The Worker uses `serde_json::Value` rather than a `#[derive(Deserialize)]` struct for the raw input because the `location` field is polymorphic (string or integer). After parsing and type detection, the values are extracted into the typed `SerpRequest` struct with defaults applied.

```rust
pub async fn parse_and_validate_body(req: &mut Request) -> std::result::Result<SerpRequest, Response> {
    // req.text() can fail if the body is not valid UTF-8, if the body stream
    // errors, or if the body has already been consumed. All of these are
    // reported as "missing_body" — the caller sent something we can't read.
    let body_text = req.text().await
        .map_err(|_| json_error(400, "Failed to read request body (body may be missing, empty, or not valid UTF-8)", "missing_body").unwrap())?;

    // In the real code, everything below lives in validate_body(&body_text).
    if body_text.trim().is_empty() {
        return Err(json_error(400, "Request body is empty", "missing_body").unwrap());
    }

    let body: serde_json::Value = serde_json::from_str(&body_text)
        .map_err(|_| json_error(400, "Invalid JSON in request body", "invalid_json").unwrap())?;

    // Validate keyword
    let keyword = body.get("keyword")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| json_error(400, "Missing or empty 'keyword' field", "missing_keyword").unwrap())?;

    // Count characters, not UTF-8 bytes: `len()` would reject non-Latin keywords early.
    if keyword.chars().count() > 700 {
        return Err(json_error(400, "Keyword exceeds 700 character limit", "invalid_keyword").unwrap());
    }

    // Validate location (polymorphic: string or integer)
    let location = match body.get("location") {
        Some(serde_json::Value::Number(n)) => {
            n.as_i64()
                .filter(|&code| code > 0)
                .map(Location::Code)
                .ok_or_else(|| json_error(400, "Location code must be a positive integer", "invalid_location").unwrap())?
        }
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                return Err(json_error(400, "Location name must not be empty", "invalid_location").unwrap());
            }
            Location::Name(trimmed)
        }
        _ => return Err(json_error(400, "Missing or invalid 'location' field", "missing_location").unwrap()),
    };

    // Validate depth (default: 10)
    let depth = match body.get("depth") {
        Some(v) => {
            let d = v.as_u64()
                .and_then(|d| u32::try_from(d).ok())
                .filter(|&d| (1..=200).contains(&d))
                .ok_or_else(|| json_error(400, "Depth must be an integer between 1 and 200", "invalid_depth").unwrap())?;
            d
        }
        None => 10,
    };

    // Validate device (default: "desktop")
    let device = match body.get("device") {
        Some(v) => {
            let d = v.as_str()
                .map(|s| s.to_lowercase())
                .filter(|s| s == "desktop" || s == "mobile")
                .ok_or_else(|| json_error(400, "Device must be 'desktop' or 'mobile'", "invalid_device").unwrap())?;
            d
        }
        None => "desktop".to_string(),
    };

    // Validate language (default: "en")
    let language = match body.get("language") {
        Some(v) => {
            let l = v.as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| json_error(400, "Language must be a non-empty string", "invalid_language").unwrap())?;
            l
        }
        None => "en".to_string(),
    };

    // Validate ai_optimized (default: false)
    // IMPORTANT: Do NOT silently coerce non-boolean types to false.
    // If the field is present, it MUST be a JSON boolean (true or false).
    // Strings like "true", integers like 1, and null are rejected with 400.
    let ai_optimized = match body.get("ai_optimized") {
        Some(v) => {
            v.as_bool()
                .ok_or_else(|| json_error(400, "ai_optimized must be a boolean (true or false)", "invalid_ai_optimized").unwrap())?
        }
        None => false,
    };

    Ok(SerpRequest {
        keyword,
        location,
        depth,
        device,
        language,
        ai_optimized,
    })
}
```

### 6.3 `src/dataforseo.rs` — DataForSEO Client

**Constants:**

```rust
const TASK_POST_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/task_post";
const TASK_GET_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/task_get/advanced";
const HIGH_PRIORITY: u8 = 2;

const FIRST_POLL_AFTER_MS: u64 = 10_000; // first result request
const POLL_INTERVAL_MS: u64 = 6_000; // between result requests
const DEADLINE_MS: u64 = 280_000; // overall limit, from just before task_post
const POLL_TIMEOUT_MS: u32 = 10_000; // longest wait for one result request, body included
const MAX_POLL_FAILURES: u32 = 3; // failed result requests in a row
const MAX_SUBREQUESTS: u64 = 1 + (DEADLINE_MS - FIRST_POLL_AFTER_MS) / POLL_INTERVAL_MS + 1;

// Checked when compiling: Workers Free allows 50 subrequests per invocation,
// and the Worker's answer (at worst, one result request that starts at the
// deadline and times out) must come at least 10 seconds before the 300-second
// client timeout, leaving time for the request's travel and the download.
const _: () = assert!(MAX_SUBREQUESTS <= 50);
const _: () = assert!(DEADLINE_MS + POLL_TIMEOUT_MS as u64 <= 290_000);

const TASK_CREATED: i64 = 20100;
const PENDING_STATUS_CODES: [i64; 2] = [40601, 40602]; // Task Handed, Task In Queue
const BUSY_STATUS_CODES: [i64; 2] = [40202, 40209]; // rate limits, sent with HTTP 200
const FIRST_SERVER_ERROR: i64 = 50000; // DataForSEO server errors, sent with HTTP 200

/// The status codes of a task_get response, and nothing else. Deserializing
/// into this skips the rest of the body without building it.
#[derive(Deserialize)]
struct StatusCodes {
    status_code: Option<i64>,
    tasks: Option<Vec<TaskStatusCode>>,
}

#[derive(Deserialize)]
struct TaskStatusCode {
    status_code: Option<i64>,
}
```

**Functions:**

```rust
/// The task_post body: one task with priority 2 and the three always-on flags.
fn build_payload(request: &SerpRequest) -> serde_json::Value

/// tasks[0].id from a task_post response, only if tasks[0].status_code is 20100.
fn created_task_id(body: &serde_json::Value) -> Option<&str>

/// The task_get/advanced URL for the task, with `.ai` when ai_optimized is set.
/// None unless the ID is a 36-character UUID (hex digits and dashes).
fn task_get_url(task_id: &str, ai_optimized: bool) -> Option<String>

/// Whether a task_get response has status 40601 or 40602: tasks[0].status_code
/// in the standard format, the top-level status_code in the .ai format.
fn is_pending(codes: &StatusCodes, ai_optimized: bool) -> bool

/// Whether the top-level status_code is 40202 or 40209: DataForSEO's rate
/// limits turned the request away, which says nothing about the task.
fn is_busy(codes: &StatusCodes) -> bool

/// Whether the top-level status_code is 50000 or higher: a DataForSEO server
/// error, not news of the task.
fn is_server_error(codes: &StatusCodes) -> bool

/// Whether a failed request's error message contains "timeout" or "timed out".
fn mentions_timeout(message: &str) -> bool

/// A request that failed before its whole response arrived: 504
/// dataforseo_timeout when mentions_timeout, otherwise 502 dataforseo_error
/// with the HTTP status received so far (0 if none).
fn request_failed(status: u16, message: String) -> Response

/// One outbound request; returns the HTTP status and body text. A failed fetch
/// or body read becomes request_failed. With timeout_ms, the request is
/// abandoned (AbortSignal.timeout) if the whole response has not arrived in
/// time; workerd then reports "TimeoutError: The operation was aborted due to
/// timeout", for a late body as for late headers.
async fn send(url: &str, method: Method, auth_header: &str, body: Option<String>,
    timeout_ms: Option<u32>) -> std::result::Result<(u16, String), Response>

/// One task_get request, limited to POLL_TIMEOUT_MS: Ok(Some(body)) when
/// finished, Ok(None) while pending or busy, Err for a failed fetch or timeout,
/// a non-2xx status, a body without DataForSEO's status codes, or a server error.
async fn poll_once(url: &str, auth_header: &str, ai_optimized: bool)
    -> std::result::Result<Option<String>, Response>

/// Wraps a DataForSEO body, unchanged, in a 200 JSON response.
fn pass_through(body_text: String) -> std::result::Result<Response, Response>

/// Sets the task, polls for its result, and returns it (or an error response).
pub async fn fetch_serp(request: &SerpRequest, login: &str, password: &str)
    -> std::result::Result<Response, Response>
```

**`fetch_serp`:**

```rust
pub async fn fetch_serp(
    request: &SerpRequest,
    login: &str,
    password: &str,
) -> std::result::Result<Response, Response> {
    let credentials = format!("{}:{}", login, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    let auth_header = format!("Basic {}", encoded);

    // 1. Set the task.
    let started_ms = Date::now().as_millis();
    let payload = build_payload(request).to_string();
    // No time limit: abandoning task_post could leave a paid task without its ID.
    let (status, post_text) = send(
        TASK_POST_URL,
        Method::Post,
        &auth_header,
        Some(payload),
        None,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(dataforseo_error(status, &post_text, None).unwrap());
    }
    let post_body: serde_json::Value = serde_json::from_str(&post_text)
        .map_err(|_| dataforseo_error(status, &post_text, None).unwrap())?;

    // 2. A task that was not created carries DataForSEO's error: pass it through.
    let Some(task_id) = created_task_id(&post_body) else {
        return pass_through(post_text);
    };
    let Some(task_url) = task_get_url(task_id, request.ai_optimized) else {
        return Err(dataforseo_error(status, &post_text, None).unwrap());
    };

    // 3. Poll until the task is finished, three requests in a row fail, or the
    //    deadline passes. Delay is a timer; waiting is not CPU time.
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
```

**Unit tests** (native, `cargo test`): the task_post body (priority and the three flags, for both formats and both location types), reading the task ID only from a created task, task_get URLs for both formats and rejection of non-UUID IDs, the pending check for both formats, the server-error check (top-level only), the rate-limit check, recognizing timeout messages whether the headers or the body were late, and reading status codes from a full result while rejecting a non-JSON body.

**Security notes:**
- The `login` and `password` are never logged or included in error responses.
- The `keyword` is not logged to avoid leaking potentially sensitive search queries.
- Only the DataForSEO HTTP status code and a truncated response body are included in error responses.

### 6.4 `src/errors.rs` — Error Response Builders

**Functions:**

```rust
/// Build a generic JSON error response.
/// `status` is the HTTP status code for the Worker's response (e.g., 400, 401, 404, 405, 504).
pub fn json_error(status: u16, error: &str, code: &str) -> Result<Response>

/// Build a DataForSEO-specific error response (always HTTP 502).
/// `dfs_status` is the HTTP status code that DataForSEO returned. Pass `0` when the
/// outbound fetch failed before receiving any HTTP response (e.g., DNS failure, network
/// error, TLS handshake failure). Callers seeing `"dataforseo_status": 0` in the JSON
/// body should interpret it as "the request never reached DataForSEO."
pub fn dataforseo_error(
    dfs_status: u16,
    dfs_body: &str,
    dfs_error_code: Option<&str>,
) -> Result<Response>

/// Build the response for a task that did not finish before the Worker's
/// deadline (always HTTP 504, code "dataforseo_timeout"). The body adds
/// "dataforseo_task_id", because DataForSEO may still finish and charge for it.
pub fn task_timeout_error(task_id: &str) -> Result<Response>

/// Truncate a string to max_bytes at a UTF-8 character boundary.
fn truncate(s: &str, max_bytes: usize) -> &str
```

**`json_error` implementation:**

```rust
pub fn json_error(status: u16, error: &str, code: &str) -> Result<Response> {
    let body = serde_json::json!({
        "error": error,
        "code": code,
    });
    let mut resp = Response::ok(body.to_string())?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp.with_status(status))
}
```

**`dataforseo_error` implementation:**

```rust
const MAX_BODY_BYTES: usize = 4096;

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
```

**UTF-8-safe truncation** (from rusty-gateway):

```rust
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
```

---

## 7. Security

### 7.1 Authentication

- **csvkey**: Shared secret validated via constant-time comparison. Passed as query parameter.
- **DataForSEO**: HTTP Basic Auth with credentials stored as Cloudflare Worker secrets.

### 7.2 Credential Protection

- Secrets are never logged, serialized to JSON, or included in error responses.
- DataForSEO credentials are base64-encoded only at the moment of request construction and never stored in encoded form.
- The Worker does not log request bodies (which contain the keyword) or response bodies (which contain SERP data).

### 7.3 Input Sanitization

- Keyword length is capped at 700 characters (DataForSEO's limit).
- Location values are type-checked (integer or non-empty string).
- Device is validated against a whitelist (`desktop`, `mobile`).
- Depth is range-checked (1-200).
- No SQL, no database, no file system access — pure request proxy.

### 7.4 Known Limitations

- **csvkey in URL**: The `csvkey` appears in the URL query string, which may be logged by intermediary proxies or in Cloudflare access logs. This is an accepted trade-off for consistency with the existing tool ecosystem (rusty-gateway, edgentities). Agents should use HTTPS exclusively.
- **No rate limiting**: The Worker does not implement its own rate limiting. It relies on DataForSEO's built-in rate limits (per DataForSEO's errors appendix in November 2026: 10,000 requests per minute and 30 simultaneous requests, for the whole account). Polling adds at most 46 short result requests per call; if a limit turns one away, the Worker asks again (Section 4.5). A compromised csvkey could exhaust the DataForSEO account balance.
- **Single csvkey**: The Worker supports one csvkey per environment. Multiple keys (comma-separated, like edgentities) can be added as a future enhancement if needed.

---

## 8. Logging

The Worker follows rusty-gateway's logging philosophy: minimal, security-conscious, never log sensitive data.

**What is logged:**
- Request method and path (e.g., `POST /v1/serp`)
- DataForSEO HTTP status code on error
- Worker-level error codes on validation failures

**What is never logged:**
- The `csvkey` value
- The `keyword` value
- DataForSEO credentials
- Full request or response bodies
- Full URLs (which may contain csvkey in query string)

Logging uses `worker::console_log!()` which outputs to Cloudflare Workers' log stream (visible via `wrangler tail`).

---

## 9. Testing

### 9.1 Manual Testing

**Health check:**
```bash
curl https://rusty-serp.<domain>/v1/health
# Expected: {"status":"ok"}
```

**Missing csvkey:**
```bash
curl -X POST https://rusty-serp.<domain>/v1/serp \
  -H "Content-Type: application/json" \
  -d '{"keyword": "test", "location": 2840}'
# Expected: 400, {"error":"Missing csvkey query parameter","code":"missing_csvkey"}
```

**Invalid csvkey:**
```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=wrong" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "test", "location": 2840}'
# Expected: 401, {"error":"Invalid csvkey","code":"unauthorized"}
```

**Valid request (standard):**
```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-key" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "rust programming", "location": 2840}'
# Expected: 200, DataForSEO standard response, after about 2.5 minutes
```

**Valid request (AI-optimized):**
```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-key" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "rust programming", "location": 2840, "ai_optimized": true}'
# Expected: 200, DataForSEO AI-optimized response (flat structure), after about 2.5 minutes
```

**Wrong method:**
```bash
curl https://rusty-serp.<domain>/v1/serp
# Expected: 405, {"error":"Method not allowed","code":"method_not_allowed"}
```

**Missing required fields:**
```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-key" \
  -H "Content-Type: application/json" \
  -d '{}'
# Expected: 400, {"error":"Missing or empty 'keyword' field","code":"missing_keyword"}
```

### 9.2 DataForSEO Sandbox

DataForSEO provides a sandbox environment for testing. Sandbox requests are free but return synthetic data. The same credentials work for both sandbox and live APIs. The sandbox endpoint is at the same URL — sandbox mode is controlled by the DataForSEO account settings, not the endpoint URL.

### 9.3 Local Development

**Important:** `wrangler secret put` sets secrets for **deployed** Workers, not for local development. For local dev with `wrangler dev`, secrets are read from a `.dev.vars` file in the project root.

**Step 1: Create `.dev.vars` file** (this file is gitignored and must never be committed):

```
CSVKEY=your-local-test-key
DATAFORSEO_LOGIN=your-dataforseo-email@example.com
DATAFORSEO_PASSWORD=your-dataforseo-api-password
```

**Step 2: Verify `.gitignore` includes `.dev.vars`** to prevent accidental credential commits:

```
# Add to .gitignore if not already present
.dev.vars
```

**Step 3: Start the local dev server:**

```bash
wrangler dev
```

This starts a local server (typically at `http://localhost:8787`). You can then test with:

```bash
curl -X POST "http://localhost:8787/v1/serp?csvkey=your-local-test-key" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "test query", "location": 2840}'
```

**Wrangler version:** Use a current Wrangler (`npm install -g wrangler` updates it). `worker` 0.9 relies on the runtime's `tracing.getActiveSpan` and `tracing.withActiveSpan` for every request. Older Wrangler releases bundle a runtime without them, and then every request fails with HTTP 500 and `Uncaught Error: Critical error` in the log. Wrangler 4.167.0 works; 4.96.0 does not.

**Note:** `wrangler secret put` is only used for setting secrets on deployed environments (staging, production). See Section 10 for deployment instructions.

---

## 10. Deployment

### 10.1 Initial Setup

```bash
# 1. Clone the repository
git clone <repo-url>
cd rusty-serp

# 2. Install wrangler, or update it: `wrangler dev` needs a current release (Section 9.3)
npm install -g wrangler

# 3. Authenticate with Cloudflare
wrangler login

# 4. Set production secrets (wrangler will prompt you to enter each value interactively)
wrangler secret put CSVKEY
wrangler secret put DATAFORSEO_LOGIN
wrangler secret put DATAFORSEO_PASSWORD

# 5. Deploy to production: push to `main` (Workers Builds builds and deploys it),
#    or deploy by hand
wrangler deploy
```

**Important:** The top-level config in `wrangler.toml` is production (Section 5.2). A bare `wrangler deploy` and `wrangler deploy --env production` both deploy to the production `rusty-serp` Worker, as does every push to `main`. Use `--env staging` for anything that must not reach production.

### 10.2 Environment-Specific Deployment

```bash
# --- Staging ---

# Set staging secrets (only needed once, or when rotating credentials)
wrangler secret put CSVKEY --env staging
wrangler secret put DATAFORSEO_LOGIN --env staging
wrangler secret put DATAFORSEO_PASSWORD --env staging

# Deploy to staging (Worker name: rusty-serp-staging)
wrangler deploy --env staging

# --- Production (Worker name: rusty-serp) ---

# Set production secrets (only needed once, or when rotating credentials).
# Without --env and with --env production, these target the same Worker.
wrangler secret put CSVKEY
wrangler secret put DATAFORSEO_LOGIN
wrangler secret put DATAFORSEO_PASSWORD

# Deploy to production. Normally a push to `main` does this through Workers Builds.
wrangler deploy
```

There is no dev Worker. Use `wrangler dev` for local testing and `--env staging` for a deployed test copy.

### 10.3 Monitoring

```bash
# Tail production logs
wrangler tail

# Tail staging logs
wrangler tail --env staging
```

---

## 11. Agent Integration Guide

### 11.1 Tool Definition (LangChain/LangGraph)

Agents should define rusty-serp as a tool with the following schema:

```python
from langchain_core.tools import tool
from pydantic import BaseModel, Field

class SerpInput(BaseModel):
    keyword: str = Field(description="The Google search query to execute")
    location: int | str = Field(description="DataForSEO location code (int) or name (str)")
    depth: int = Field(default=10, description="Number of results to retrieve (1-200)")
    device: str = Field(default="desktop", description="Device type: 'desktop' or 'mobile'")
    language: str = Field(default="en", description="Language code for search results")
    ai_optimized: bool = Field(default=False, description="Use AI-optimized response format")

@tool(args_schema=SerpInput)
def fetch_serp(keyword: str, location: int | str, depth: int = 10, 
               device: str = "desktop", language: str = "en", 
               ai_optimized: bool = False) -> dict:
    """Fetch live Google SERP results from DataForSEO via the rusty-serp proxy."""
    import httpx
    response = httpx.post(
        f"https://rusty-serp.<domain>/v1/serp?csvkey={CSVKEY}",
        json={
            "keyword": keyword,
            "location": location,
            "depth": depth,
            "device": device,
            "language": language,
            "ai_optimized": ai_optimized,
        },
        timeout=300,
    )
    response.raise_for_status()
    return response.json()
```

### 11.2 Tool Definition (Deep Agents)

```python
from deep_agents import create_deep_agent, Tool

serp_tool = Tool(
    name="fetch_serp",
    description="Fetch live Google SERP results including AI Overview, organic results, People Also Ask, and related searches",
    endpoint="https://rusty-serp.<domain>/v1/serp",
    method="POST",
    auth={"type": "query_param", "key": "csvkey", "value_env": "RUSTY_SERP_KEY"},
    input_schema={
        "keyword": {"type": "string", "required": True},
        "location": {"type": ["integer", "string"], "required": True},
        "depth": {"type": "integer", "default": 10},
        "device": {"type": "string", "enum": ["desktop", "mobile"], "default": "desktop"},
        "language": {"type": "string", "default": "en"},
        "ai_optimized": {"type": "boolean", "default": False},
    },
)
```

### 11.3 Best Practices for Agents

1. **Use `ai_optimized: true` by default** when the agent is consuming the SERP data for analysis. The flat structure with stripped null fields reduces token usage significantly.
2. **Use `ai_optimized: false`** when the agent needs full metadata (rectangles, XPath, exact position data, cost breakdown).
3. **Set `depth` conservatively**. Start with `10` (default). Only increase if the agent explicitly needs more results. Each additional 10 results adds ~$0.0012.
4. **Handle DataForSEO task-level errors**. A `200` HTTP response does not guarantee SERP success. Always check the `status_code` field in the response body (should be `20000` for success).
5. **Set the agent tool timeout to 300 seconds**. The Worker always turns on redirect resolution (`resolve_goto_urls`), which DataForSEO caps at 260 seconds of turnaround, and the poisoned-result check (`resolve_poisoned_results`), which adds one more Google request. In testing, requests took 145 to 186 seconds. The Worker gives up at 280 seconds and answers `504` `dataforseo_timeout`, so a 300-second timeout always gets the Worker's answer (Section 4.6).
6. **Handle non-JSON error responses**. If the Worker throws an exception or the connection drops, the agent receives a Cloudflare HTML error page or a connection reset, not a JSON response. Agents must handle this gracefully (see Section 4.6 for details).
7. **Send `ai_optimized` as a JSON boolean**, not a string. `"ai_optimized": true` is correct. `"ai_optimized": "true"` returns a `400` error.

---

## 12. File Tree (Final)

```
rusty-serp/
  .dev.vars                  # Local dev secrets (CSVKEY, DATAFORSEO_LOGIN, DATAFORSEO_PASSWORD)
                             # *** GITIGNORED — never commit this file ***
  .gitignore                 # Must include: .dev.vars, target/, build/, node_modules/
  Cargo.toml                 # Rust package manifest (5 dependencies)
  Cargo.lock                 # Dependency lock file (generated)
  wrangler.toml              # Cloudflare Worker configuration (top-level name: rusty-serp, production)
  LICENSE                    # Project license
  README.md                  # Project overview and usage
  prd/
    rusty-serp-prd.md        # This PRD document
  src/
    lib.rs                   # Entry point (#[event(fetch)]), routing (GET/HEAD health, POST serp), orchestration
    validation.rs            # csvkey auth, JSON body parsing, field validation (including ai_optimized type check)
    dataforseo.rs            # DataForSEO API client (task_post, polling task_get, response handling, deadline)
    errors.rs                # json_error(), dataforseo_error(), task_timeout_error(), truncate()
```

**Total source files:** 4 Rust files.
**Total dependencies:** 5 crates.
**Estimated WASM binary size:** <500 KB (based on rusty-gateway's similar profile).

---

## Appendix A: DataForSEO SERP Item Types Reference

The following item types may appear in the `items` array of a DataForSEO SERP response. Agents should be prepared to encounter any of these:

| Item Type | Description |
|-----------|-------------|
| `ai_overview` | Google's AI-generated overview (always captured due to `load_async_ai_overview: true`) |
| `organic` | Standard organic search results |
| `paid` | Paid/sponsored results |
| `featured_snippet` | Featured snippet (answer box with source) |
| `people_also_ask` | "People Also Ask" expandable questions |
| `related_searches` | Related search suggestions |
| `knowledge_graph` | Knowledge panel (may contain nested `ai_overview`) |
| `local_pack` | Local business listings |
| `video` | Video results |
| `images` | Image results |
| `top_stories` | News/top stories carousel |
| `shopping` | Shopping/product results |
| `carousel` | Entity carousel |
| `answer_box` | Direct answer box |
| `twitter` | Twitter/X results |
| `events` | Event listings |
| `jobs` | Job listings |
| `recipes` | Recipe results |
| `discussions_and_forums` | Discussion/forum results |
| `perspectives` | Perspectives results |
| `short_videos` | Short-form video results |

For complete item schemas, refer to the [DataForSEO SERP API documentation](https://docs.dataforseo.com/v3/serp/google/organic/task_get/advanced/).

---

## Appendix B: DataForSEO Status Codes Reference

| Code | Meaning | Agent Action |
|------|---------|-------------|
| `20000` | Success | Process results |
| `40100` | Not authorized | Check DATAFORSEO_LOGIN/PASSWORD secrets |
| `40102` | No search results | Return empty to user (valid query, no results) |
| `40200` | Payment required | Alert operator, DataForSEO balance depleted |
| `40202` | Rate limit exceeded | From task_post: retry after delay (no task was set). Never seen from a result request: the Worker waits and asks again |
| `40209` | Too many simultaneous requests (more than 30) | Same as `40202` |
| `40210` | Insufficient funds | Alert operator, DataForSEO balance too low |
| `40501` | Invalid field | Check request construction |
| `40601` / `40602` | Task handed / in queue | Never seen by agents: the Worker keeps polling while a task has these codes |
| `50000` and up | DataForSEO server error | From task_post: retry after delay (no task was set). On a result request the Worker retries it; after three in a row it answers `502` `dataforseo_error` |

The Live endpoint's `50401` (120-second timeout) does not apply: the Worker uses `task_post` and `task_get`, and reports its own deadline as HTTP `504` `dataforseo_timeout` (Section 4.6).

---

## Appendix C: Common Location Codes

| Location | Code | Name |
|----------|------|------|
| United States | `2840` | `"United States"` |
| United Kingdom | `2826` | `"United Kingdom"` |
| Canada | `2124` | `"Canada"` |
| Australia | `2036` | `"Australia"` |
| Germany | `2276` | `"Germany"` |
| France | `2250` | `"France"` |
| India | `2356` | `"India"` |
| Japan | `2392` | `"Japan"` |
| Brazil | `2076` | `"Brazil"` |
| New York, US | `1023191` | `"New York,New York,United States"` |
| London, UK | `1006886` | `"London,England,United Kingdom"` |
| Los Angeles, US | `1013962` | `"Los Angeles,California,United States"` |

Full list available via DataForSEO locations endpoint: `GET https://api.dataforseo.com/v3/serp/google/locations`
