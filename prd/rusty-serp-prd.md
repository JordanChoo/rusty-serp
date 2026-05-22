# rusty-serp: Product Requirements Document

## 1. Overview

### 1.1 Purpose

rusty-serp is a Rust-based Cloudflare Worker that serves as a stateless HTTP proxy to the DataForSEO SERP Google Organic Live Advanced API. It provides a unified, authenticated endpoint that Graph Agents (LangGraph, LangChain, Deep Agents, and similar orchestration frameworks) call as a tool to retrieve live Google SERP data.

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
- Construct and execute DataForSEO SERP API requests
- Route to standard or AI-optimized (`.ai`) endpoint based on `ai_optimized` flag
- Always set `load_async_ai_overview: true` in outbound requests
- Return DataForSEO responses verbatim
- Health check endpoint

**Out of scope:**
- Response caching (no KV, no Cache API)
- Response transformation or filtering
- Rate limiting (rely on DataForSEO's built-in limits)
- Batch/queue task mode (Live mode only)
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
                                                                  | POST (Basic Auth)
                                                                  |
                                                                  v
                                                    +----------------------------+
                                                    |        DataForSEO          |
                                                    | /v3/serp/google/organic/   |
                                                    |   live/advanced[.ai]       |
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
   - depth: optional, integer 1-700, default 10
   - device: optional, "desktop" or "mobile", default "desktop"
   - language: optional, non-empty string, default "en"
   - ai_optimized: optional, must be boolean if present, default false
9. Worker reads DATAFORSEO_LOGIN and DATAFORSEO_PASSWORD from environment (reject missing with 500)
10. Worker constructs DataForSEO request body (always sets load_async_ai_overview: true)
11. Worker determines endpoint URL:
    - ai_optimized == true  -> .../live/advanced.ai
    - ai_optimized == false -> .../live/advanced
12. Worker sends POST to DataForSEO with Basic Auth
13. Worker reads DataForSEO response
14. Worker returns DataForSEO response body verbatim with status 200 (or 502/504 on failure)
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
    dataforseo.rs            # DataForSEO API client (request construction, fetch, response handling)
    errors.rs                # JSON error response builders
```

**Module responsibilities:**

| Module | Responsibility | Imports from |
|--------|---------------|-------------|
| `lib.rs` | `#[event(fetch)]` entry point, HTTP method check, orchestrates validation -> dataforseo call chain | `validation`, `dataforseo`, `errors` |
| `validation.rs` | Parses `csvkey` from query params, reads and validates JSON body, performs constant-time auth comparison, validates all input fields | `errors` |
| `dataforseo.rs` | Constructs DataForSEO request body (always setting `load_async_ai_overview: true`), builds Basic Auth header, selects endpoint URL, executes fetch, handles HTTP-level errors | `errors` |
| `errors.rs` | Provides `json_error()` and `dataforseo_error()` response builder functions, UTF-8-safe body truncation | (none) |

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
| `depth` | integer | No | `10` | Range: 1-700. Charged per 10 results by DataForSEO. | Number of SERP results to retrieve. |
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

The Worker returns the DataForSEO response body verbatim. The response shape depends on the `ai_optimized` flag:

**When `ai_optimized: false` (standard response):**

```json
{
  "version": "0.1.20250520",
  "status_code": 20000,
  "status_message": "Ok.",
  "time": "1.2345 sec.",
  "cost": 0.004,
  "tasks_count": 1,
  "tasks_error": 0,
  "tasks": [
    {
      "id": "05281810-1535-0121-0000-014aadae6b3a",
      "status_code": 20000,
      "status_message": "Ok.",
      "time": "1.1234 sec.",
      "cost": 0.004,
      "result_count": 1,
      "path": ["v3", "serp", "google", "organic", "live", "advanced"],
      "data": {
        "api": "serp",
        "function": "live",
        "se": "google",
        "se_type": "organic",
        "keyword": "best project management tools 2026",
        "location_code": 2840,
        "language_code": "en",
        "device": "desktop",
        "depth": 20,
        "load_async_ai_overview": true
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

Note: The AI-optimized format strips `null`/`false`/empty fields, removes `position` and `xpath`, flattens the envelope, and rounds floats to 3 decimal places. This is handled entirely by DataForSEO's `.ai` endpoint — the Worker passes it through unchanged.

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
| 400 | `invalid_depth` | `depth` is not an integer in range 1-700 |
| 400 | `invalid_device` | `device` is not `"desktop"` or `"mobile"` |
| 400 | `invalid_language` | `language` is empty or not a string |
| 400 | `invalid_ai_optimized` | `ai_optimized` is present but is not a JSON boolean (`true`/`false`) |
| 401 | `unauthorized` | `csvkey` does not match the `CSVKEY` secret |
| 404 | `not_found` | Unrecognized path |
| 405 | `method_not_allowed` | Non-POST method on `/v1/serp`, non-GET/HEAD on `/v1/health` |
| 500 | `missing_config` | `CSVKEY`, `DATAFORSEO_LOGIN`, or `DATAFORSEO_PASSWORD` secret is not configured |
| 502 | `dataforseo_error` | DataForSEO returned an HTTP error (non-2xx status), **or** the outbound fetch to DataForSEO failed entirely (network error, DNS failure, TLS error). When the fetch never reached DataForSEO, the response will have `"dataforseo_status": 0` — this tells the caller the failure was at the network level, not a DataForSEO HTTP error. |
| 504 | `dataforseo_timeout` | The Worker's outbound fetch to DataForSEO did not complete within the Worker's wall-clock limit. **Important implementation note:** Cloudflare Workers do not support `tokio::time::timeout` or similar async timeouts. Instead, when the Worker exceeds Cloudflare's wall-clock limit (30 seconds on free plan, configurable on paid plans), Cloudflare kills the Worker process. The Worker **cannot** catch this and return a structured `504` response. Therefore, this error code is **only** produced when the Worker can detect a timeout itself — specifically, when the `Fetch` call returns an error whose message contains "timeout" or "timed out". In practice, most timeouts will surface to the caller as Cloudflare's generic `524` (a timeout occurred) or a connection reset, **not** as this structured `504` JSON. Agents should handle both: (a) a structured `504` JSON response with code `dataforseo_timeout`, and (b) a non-JSON `524` or connection error from Cloudflare. |

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

| `ai_optimized` Value | DataForSEO Endpoint |
|----------------------|-------------------|
| `false` (default) | `https://api.dataforseo.com/v3/serp/google/organic/live/advanced` |
| `true` | `https://api.dataforseo.com/v3/serp/google/organic/live/advanced.ai` |

### 4.2 Authentication

DataForSEO uses HTTP Basic Authentication. The Worker constructs the `Authorization` header from two secrets:

```
Authorization: Basic base64(DATAFORSEO_LOGIN:DATAFORSEO_PASSWORD)
```

The base64 encoding is performed at request time using the `base64` crate. Credentials are never logged, cached, or included in error responses.

### 4.3 Request Construction

The Worker constructs the DataForSEO request body as a JSON array containing a single task object. The following fields are always set:

| DataForSEO Field | Source | Notes |
|-----------------|--------|-------|
| `keyword` | `request.keyword` | Passed through as-is |
| `location_code` | `request.location` (when integer) | Set when location is numeric |
| `location_name` | `request.location` (when string) | Set when location is a string |
| `depth` | `request.depth` (default: `10`) | Passed through |
| `device` | `request.device` (default: `"desktop"`) | Normalized to lowercase |
| `language_code` | `request.language` (default: `"en"`) | Passed through |
| `load_async_ai_overview` | **Always `true`** | Hardcoded. Never overridden by caller input. |

**Example outbound request body:**

```json
[
  {
    "keyword": "best project management tools 2026",
    "location_code": 2840,
    "depth": 20,
    "device": "desktop",
    "language_code": "en",
    "load_async_ai_overview": true
  }
]
```

### 4.4 Outbound Request Headers

| Header | Value |
|--------|-------|
| `Authorization` | `Basic <base64(login:password)>` |
| `Content-Type` | `application/json` |

### 4.5 Response Handling

1. **Read the response body as text.** This is done regardless of status code to capture error details.
2. **Check HTTP status code.** If not in the 2xx range, return a `502` `dataforseo_error` response with the upstream status and truncated body.
3. **If 2xx**, return the response body verbatim as the Worker response with:
   - Status: `200`
   - `Content-Type: application/json`

The Worker does not parse, validate, or transform the DataForSEO response body. It is treated as an opaque JSON string passed through to the caller.

### 4.6 Timeout Handling

DataForSEO's Live mode has a 120-second timeout per task. The Worker does not impose its own timeout — it relies on:
1. DataForSEO's internal 120-second task timeout (status code `50401`)
2. Cloudflare Workers' 30-second wall-clock limit on the free plan, or configurable limit on paid plans

**What actually happens in each scenario:**

| Scenario | What the caller sees |
|----------|---------------------|
| DataForSEO responds within Cloudflare's wall-clock limit | Normal `200` response with DataForSEO's JSON body. If DataForSEO itself timed out, the JSON body will have `status_code: 50401` — the Worker still returns HTTP `200` (pass-through). |
| The `Fetch` call errors with a message containing "timeout" or "timed out" | The Worker returns a structured `504` JSON response with code `dataforseo_timeout`. This is rare — it requires the `Fetch` API itself to report a timeout error before Cloudflare kills the Worker. |
| Cloudflare's wall-clock limit is exceeded before the fetch completes | Cloudflare **kills the Worker process**. The Worker cannot catch this. The caller receives Cloudflare's generic `524 A Timeout Occurred` HTML error page — **not** a structured JSON response. There is no way to produce a JSON `504` in this case. |

**Implication for agents:** Agents calling rusty-serp should handle three timeout scenarios: (1) a `200` with `status_code: 50401` in the body (DataForSEO timeout), (2) a `504` JSON with code `dataforseo_timeout` (rare, fetch-level timeout), and (3) a non-JSON `524` or connection reset (Cloudflare killed the Worker). Set agent tool timeout to at least 120 seconds.

**Implication for deployment:** If you are on the Cloudflare free plan (30-second limit), most DataForSEO queries will complete in time. But deep queries (`depth > 100`) or complex operator queries may exceed 30 seconds. Use a paid plan with Unbound (no CPU time limit) or Standard (30-second default, configurable) if you need to support long-running queries.

### 4.7 Cost Implications

| Scenario | DataForSEO Cost |
|----------|----------------|
| Basic query, 10 results | $0.002 (base) + $0.002 (`load_async_ai_overview`) = **$0.004** |
| Basic query, 20 results | $0.004 (2 pages) + $0.002 = **$0.006** |
| AI overview absent from SERP | `load_async_ai_overview` surcharge refunded |
| AI overview was cached (not async) | `load_async_ai_overview` surcharge refunded |
| Query with search operators | Base cost x5 |
| `.ai` suffix (AI-optimized) | No additional cost |

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
name = "rusty-serp-dev"
main = "build/worker/shim.mjs"
compatibility_date = "2026-05-20"

[build]
command = "curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable && . \"$HOME/.cargo/env\" && rustup target add wasm32-unknown-unknown && cargo install -q worker-build && worker-build --release"

[env.staging]
name = "rusty-serp-staging"

[env.production]
name = "rusty-serp"
```

**Key design decisions:**
- **Self-bootstrapping build command**: Installs Rust, the WASM target, and `worker-build` if not already present. Identical pattern to rusty-gateway.
- **No `[vars]` section**: All configuration is via secrets. No environment variables are needed for this Worker.
- **Staging and production environments**: Allows separate secret sets for testing vs. production.
- **Top-level `name` is `rusty-serp-dev`**: The top-level name is used for bare `wrangler deploy` (no `--env` flag) and local `wrangler dev`. It is intentionally different from the `[env.production]` name (`rusty-serp`) to prevent accidental production deployments. A bare `wrangler deploy` deploys to the `rusty-serp-dev` Worker, which is harmless. To deploy to production, you **must** explicitly pass `--env production`. If the top-level name were also `rusty-serp`, a bare `wrangler deploy` would overwrite the production Worker.

### 5.3 Cargo Configuration

```toml
[package]
name = "rusty-serp"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
worker = "0.8"
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
| `worker` | `0.8` | Cloudflare Workers runtime bindings. Provides `Request`, `Response`, `Env`, `Fetch`, `Headers`, `Method`. Same version as rusty-gateway. |
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
| `depth` | If present, must be integer in range 1-700. | `invalid_depth` |
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

    if keyword.len() > 700 {
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
                .filter(|&d| d >= 1 && d <= 700)
                .ok_or_else(|| json_error(400, "Depth must be an integer between 1 and 700", "invalid_depth").unwrap())?;
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
const DATAFORSEO_BASE_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/live/advanced";
const DATAFORSEO_AI_URL: &str = "https://api.dataforseo.com/v3/serp/google/organic/live/advanced.ai";
```

**Functions:**

```rust
/// Construct the DataForSEO request, execute it, and return the response.
/// Returns the DataForSEO response body as a Worker Response with status 200,
/// or an Err(Response) with 502/504 on failure.
pub async fn fetch_serp(
    request: &SerpRequest,
    login: &str,
    password: &str,
) -> std::result::Result<Response, Response>
```

**Request construction:**

```rust
pub async fn fetch_serp(
    request: &SerpRequest,
    login: &str,
    password: &str,
) -> std::result::Result<Response, Response> {
    // 1. Select endpoint URL
    let url = if request.ai_optimized {
        DATAFORSEO_AI_URL
    } else {
        DATAFORSEO_BASE_URL
    };

    // 2. Build the task object
    let mut task = serde_json::json!({
        "keyword": request.keyword,
        "depth": request.depth,
        "device": request.device,
        "language_code": request.language,
        "load_async_ai_overview": true,
    });

    // 3. Set location field based on type
    match &request.location {
        Location::Code(code) => {
            task["location_code"] = serde_json::json!(code);
        }
        Location::Name(name) => {
            task["location_name"] = serde_json::json!(name);
        }
    }

    // 4. Wrap in array (DataForSEO expects array of tasks)
    let payload = serde_json::json!([task]);

    // 5. Build Basic Auth header
    let credentials = format!("{}:{}", login, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    let auth_header = format!("Basic {}", encoded);

    // 6. Construct outbound request
    let mut headers = Headers::new();
    headers.set("Authorization", &auth_header)?;
    headers.set("Content-Type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(payload.to_string().into()));

    let outbound = Request::new_with_init(url, &init)?;

    // 7. Execute fetch
    // If the fetch itself fails (DNS failure, network error, TLS error, etc.),
    // we return a 502 with dataforseo_status: 0. The 0 tells callers that the
    // request never reached DataForSEO — it's not an HTTP status code from them.
    //
    // If the error message contains "timeout" or "timed out", we return a 504
    // with code "dataforseo_timeout" instead. This is the only case where the
    // Worker can produce a structured 504 — most real timeouts will be killed
    // by Cloudflare's wall-clock limit before this code runs.
    let mut response = Fetch::Request(outbound).send().await
        .map_err(|e| {
            let err_msg = format!("Fetch failed: {}", e);
            let err_lower = err_msg.to_lowercase();
            if err_lower.contains("timeout") || err_lower.contains("timed out") {
                json_error(504, &err_msg, "dataforseo_timeout").unwrap()
            } else {
                dataforseo_error(0, &err_msg, None).unwrap()
            }
        })?;

    // 8. Read response body
    let status = response.status_code();
    let body_text = response.text().await
        .map_err(|e| dataforseo_error(status, &format!("Failed to read response: {}", e), None).unwrap())?;

    // 9. Check status
    if !(200..300).contains(&(status as u16)) {
        return Err(dataforseo_error(status, &body_text, None).unwrap());
    }

    // 10. Return DataForSEO response verbatim
    let mut resp = Response::ok(body_text)?;
    resp.headers_mut().set("Content-Type", "application/json")?;
    Ok(resp)
}
```

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
- Depth is range-checked (1-700).
- No SQL, no database, no file system access — pure request proxy.

### 7.4 Known Limitations

- **csvkey in URL**: The `csvkey` appears in the URL query string, which may be logged by intermediary proxies or in Cloudflare access logs. This is an accepted trade-off for consistency with the existing tool ecosystem (rusty-gateway, edgentities). Agents should use HTTPS exclusively.
- **No rate limiting**: The Worker does not implement its own rate limiting. It relies on DataForSEO's built-in rate limits (2,000 requests/minute, 30 concurrent). A compromised csvkey could exhaust the DataForSEO account balance.
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
# Expected: 200, DataForSEO standard response
```

**Valid request (AI-optimized):**
```bash
curl -X POST "https://rusty-serp.<domain>/v1/serp?csvkey=your-key" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "rust programming", "location": 2840, "ai_optimized": true}'
# Expected: 200, DataForSEO AI-optimized response (flat structure)
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

**Note:** `wrangler secret put` is only used for setting secrets on deployed environments (staging, production). See Section 10 for deployment instructions.

---

## 10. Deployment

### 10.1 Initial Setup

```bash
# 1. Clone the repository
git clone <repo-url>
cd rusty-serp

# 2. Install wrangler (if not already installed)
npm install -g wrangler

# 3. Authenticate with Cloudflare
wrangler login

# 4. Set production secrets (wrangler will prompt you to enter each value interactively)
wrangler secret put CSVKEY --env production
wrangler secret put DATAFORSEO_LOGIN --env production
wrangler secret put DATAFORSEO_PASSWORD --env production

# 5. Deploy to production
wrangler deploy --env production
```

**Important:** Always use `--env production` or `--env staging` when deploying. A bare `wrangler deploy` (no `--env` flag) deploys to the `rusty-serp-dev` Worker (the top-level name in `wrangler.toml`), which is intended for development only. See Section 5.2 for why the names are intentionally different.

### 10.2 Environment-Specific Deployment

```bash
# --- Staging ---

# Set staging secrets (only needed once, or when rotating credentials)
wrangler secret put CSVKEY --env staging
wrangler secret put DATAFORSEO_LOGIN --env staging
wrangler secret put DATAFORSEO_PASSWORD --env staging

# Deploy to staging (Worker name: rusty-serp-staging)
wrangler deploy --env staging

# --- Production ---

# Set production secrets (only needed once, or when rotating credentials)
wrangler secret put CSVKEY --env production
wrangler secret put DATAFORSEO_LOGIN --env production
wrangler secret put DATAFORSEO_PASSWORD --env production

# Deploy to production (Worker name: rusty-serp)
wrangler deploy --env production

# --- Dev (for testing only, not for agents to call) ---

# A bare deploy goes to rusty-serp-dev. Secrets must be set without --env.
wrangler secret put CSVKEY
wrangler secret put DATAFORSEO_LOGIN
wrangler secret put DATAFORSEO_PASSWORD
wrangler deploy
```

### 10.3 Monitoring

```bash
# Tail live logs
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
    depth: int = Field(default=10, description="Number of results to retrieve (1-700)")
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
        timeout=120,
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
3. **Set `depth` conservatively**. Start with `10` (default). Only increase if the agent explicitly needs more results. Each additional 10 results adds ~$0.002.
4. **Handle DataForSEO task-level errors**. A `200` HTTP response does not guarantee SERP success. Always check the `status_code` field in the response body (should be `20000` for success).
5. **Set appropriate timeouts**. DataForSEO Live mode can take up to 120 seconds. Agent tool timeout should be at least 120 seconds.
6. **Handle non-JSON timeout responses**. If Cloudflare's wall-clock limit kills the Worker, the agent receives a `524` HTML error page or a connection reset — not a JSON response. Agents must handle this gracefully (see Section 4.6 for details).
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
  wrangler.toml              # Cloudflare Worker configuration (top-level name: rusty-serp-dev)
  LICENSE                    # Project license
  README.md                  # Project overview and usage
  prd/
    rusty-serp-prd.md        # This PRD document
  src/
    lib.rs                   # Entry point (#[event(fetch)]), routing (GET/HEAD health, POST serp), orchestration
    validation.rs            # csvkey auth, JSON body parsing, field validation (including ai_optimized type check)
    dataforseo.rs            # DataForSEO API client (request build, fetch, response handling, timeout detection)
    errors.rs                # json_error(), dataforseo_error(), truncate()
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

For complete item schemas, refer to the [DataForSEO SERP API documentation](https://docs.dataforseo.com/v3/serp/google/organic/live/advanced/).

---

## Appendix B: DataForSEO Status Codes Reference

| Code | Meaning | Agent Action |
|------|---------|-------------|
| `20000` | Success | Process results |
| `40100` | Not authorized | Check DATAFORSEO_LOGIN/PASSWORD secrets |
| `40102` | No search results | Return empty to user (valid query, no results) |
| `40200` | Payment required | Alert operator, DataForSEO balance depleted |
| `40202` | Rate limit exceeded | Retry after delay |
| `40209` | Too many concurrent requests | Retry after delay |
| `40501` | Invalid field | Check request construction |
| `50401` | Timeout (120s) | Retry or reduce depth |

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
