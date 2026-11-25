# rusty-serp

A Rust-based Cloudflare Worker that serves as a stateless HTTP proxy to the [DataForSEO](https://dataforseo.com/) SERP Google Organic Advanced API. It provides a unified, authenticated endpoint that Graph Agents call as a tool to retrieve live Google SERP data, including AI Overviews, organic results, People Also Ask, and related searches.

## Why rusty-serp Exists

Graph Agents operating across multiple orchestration frameworks (LangGraph, LangChain, Deep Agents) need a consistent, low-latency, globally distributed interface to fetch live SERP data. Directly embedding DataForSEO credentials and API logic into each agent creates three problems:

1. **Credential sprawl**: every agent framework, every deployment, every developer needs a copy of the DataForSEO API credentials
2. **Inconsistent error handling**: each integration handles timeouts, auth failures, and malformed responses differently
3. **Duplicated integration code**: the same request construction, validation, and response handling logic gets reimplemented across Python, TypeScript, and other agent runtimes

rusty-serp centralizes all of this behind a single HTTP endpoint. Agents send a simple JSON POST with a keyword and location; rusty-serp handles authentication, request construction, endpoint selection, and error normalization.

## Architecture

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

### Request Flow

1. Agent sends `POST /v1/serp?csvkey=<key>` with a JSON body
2. Worker validates the csvkey via constant-time comparison
3. Worker parses and validates the request body (keyword, location, depth, device, language, ai_optimized)
4. Worker sets a high-priority DataForSEO task (`task_post` with `priority: 2`), with Basic Auth, and always sets `load_async_ai_overview: true`, `resolve_goto_urls: true` and `resolve_poisoned_results: true`
5. Worker asks `task_get/advanced` for the result 10 seconds later and then every 6 seconds until the task is done. With `ai_optimized: true` it asks `task_get/advanced/{id}.ai` instead
6. Worker returns the finished task's response verbatim, without transformation or filtering

### Why the Worker Polls

Cloudflare ends a Worker's outbound request with a `524` error when the server sends nothing for 125 seconds (its [Proxy Read Timeout](https://developers.cloudflare.com/fundamentals/reference/connection-limits/), which only Enterprise zones can raise). With redirect resolution and the poisoned-result check always on, DataForSEO needs longer than that, so a single Live request was cut off. Instead, the Worker sets a task and polls for it, and every one of those requests returns at once. Only the Worker's answer to its caller takes long, and Cloudflare doesn't cut that off: a test Worker answered after 200 seconds.

The Worker gives up after 280 seconds and answers `504` with code `dataforseo_timeout`. It abandons any result request that takes longer than 10 seconds, so it answers within about 290 seconds even if DataForSEO stops responding. If DataForSEO's rate limits (`40202`, `40209`) turn a result request away, the Worker waits and asks again, as it does for an unfinished task. If they turn the task request away, no task was set and nothing was paid, so the Worker sends it again every 6 seconds for up to a minute; after that it answers `429` with code `dataforseo_rate_limited`. One call makes at most 47 requests to DataForSEO, within the 50 that the Workers Free plan allows. The Worker reads only the status codes from each result, without building the rest of the document, so even a large result costs little CPU time.

### Module Structure

```
src/
  lib.rs           # Entry point, routing, orchestration
  validation.rs    # csvkey auth, JSON body parsing, field validation
  dataforseo.rs    # DataForSEO API client (task_post, polling task_get, response handling)
  errors.rs        # JSON error response builders, UTF-8-safe truncation
```

Each module owns a single concern. `errors.rs` has no internal dependencies; `validation.rs` and `dataforseo.rs` depend only on `errors.rs`; `lib.rs` orchestrates all three.

## Design Principles

### Pure Rust, No TypeScript Shell

rusty-serp compiles directly to `wasm32-unknown-unknown` via `worker-build`. There is no TypeScript wrapper, no WASM bridge glue, and no Node.js runtime involvement. The Worker binary is a single `.wasm` file under 500KB, optimized with `opt-level = "z"`, link-time optimization, and symbol stripping for minimal cold-start latency.

### Error-as-Value

Every function in the request pipeline returns `Result<T, Response>` where the `Err` variant is an already-constructed HTTP error response, not a custom error type. This eliminates the need for error type hierarchies, `.into()` conversions, or centralized error-to-response mapping. When something fails, the error *is* the response.

### Fail-Early Chain

Request processing follows a strict sequential pipeline that short-circuits on first failure:

```
method check → csvkey auth → body parse → field validation → secrets → API call
```

No partial processing. No "validate everything and return all errors." The first problem stops the chain and returns immediately.

### Pass-Through Responses

DataForSEO responses are returned verbatim to the caller. The Worker does not parse, validate, filter, or restructure the response payload. This means agents always get the full, unmodified DataForSEO response, and the Worker never needs to be updated when DataForSEO changes their response schema.

### Constant-Time Authentication

The csvkey comparison uses a constant-time algorithm (XOR fold) to prevent timing attacks:

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

The length check is an accepted trade-off: it leaks key length but prevents timing attacks on key content. All comparisons take the same number of operations regardless of where the first mismatch occurs.

## API Reference

### `GET /v1/health`

Unauthenticated liveness probe. Also supports `HEAD` (returns headers only, no body).

**Response:** `200 OK`

```json
{"status": "ok"}
```

### `POST /v1/serp`

Execute a live SERP query against DataForSEO. Requires `csvkey` query parameter.

**Request:**

```bash
curl -X POST "https://rusty-serp.example.com/v1/serp?csvkey=your-secret-key" \
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

**Request Fields:**

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `keyword` | string | Yes | — | Google search query (1-700 chars). Supports operators (`site:`, `intitle:`, etc.) but note 5x cost multiplier. |
| `location` | string or int | Yes | — | DataForSEO location code (e.g., `2840` for US) or name (e.g., `"United States"`). |
| `depth` | integer | No | `10` | Number of results to retrieve (1-200). Charged per 10 by DataForSEO. |
| `device` | string | No | `"desktop"` | `"desktop"` or `"mobile"` (case-insensitive). |
| `language` | string | No | `"en"` | Language code (e.g., `"en"`, `"es"`, `"fr"`, `"de"`). |
| `ai_optimized` | boolean | No | `false` | When `true`, calls the `.ai` endpoint for a flattened, token-efficient response. Must be a JSON boolean; strings like `"true"` are rejected. |

**Location field behavior:** Type detection is based strictly on the JSON value type. An integer like `2840` maps to `location_code`. A string like `"United States"` maps to `location_name`. A string like `"2840"` is treated as a location *name*, not a code. The Worker does not attempt to parse strings as integers.

**Response:** `200 OK` with DataForSEO response body passed through verbatim. The HTTP status is always `200` for successful upstream calls; check the `status_code` field in the JSON body for DataForSEO-level success or failure (`20000` = success).

The body is DataForSEO's `task_get/advanced` response for the finished task (`task_get/advanced/{id}.ai` with `ai_optimized: true`). It has the same shape as a Live response, with three differences in the standard format: `time` is how long the result request took, not the search; `path` names `task_get` and ends with the task ID; and `data` also echoes `priority`. `cost` is the task's full cost. If DataForSEO doesn't create the task at all (for example, an invalid field or an empty balance), the `task_post` response is returned instead, with the error in `tasks[0].status_code`. That response is in the standard format even when `ai_optimized` is `true`.

**Result URLs:** rusty-serp always sends `resolve_goto_urls: true`, so DataForSEO replaces Google's redirect ("goto") links in the results with the final destination URLs. In testing, that covered both `url` fields and links inside `markdown` text. A redirect DataForSEO cannot resolve is returned unchanged. Callers cannot turn this off. It makes requests slower: DataForSEO says it adds up to 40 seconds to a high-priority task, for a turnaround of at most 260 seconds.

**Organic results:** rusty-serp always sends `resolve_poisoned_results: true`. Google sometimes answers automated requests with a "poisoned" page that looks normal but has most organic listings swapped for unrelated ones. DataForSEO checks the first results page against Google's Web view. If the page looks poisoned, it replaces the affected organic results with the verified ones, appends any extra listings the Web view has, and collects any further pages (`depth` above 10) from the Web view as well. Each replaced or added result has `from_web` in its `checks` array, in both response formats. AI Overviews and other SERP features on the first page are returned as collected. Callers cannot turn this off. Every request is charged for one extra SERP page, even when nothing was poisoned, and waits for one extra Google request.

### Error Responses

All errors return a consistent JSON shape:

```json
{
  "error": "Human-readable error message",
  "code": "machine_readable_error_code"
}
```

| Status | Code | Condition |
|--------|------|-----------|
| 400 | `missing_csvkey` | `csvkey` query parameter not present |
| 400 | `missing_body` | Request body is empty or not valid UTF-8 |
| 400 | `invalid_json` | Body is not valid JSON |
| 400 | `missing_keyword` | `keyword` field absent or empty |
| 400 | `invalid_keyword` | `keyword` exceeds 700 characters |
| 400 | `missing_location` | `location` field absent or invalid type |
| 400 | `invalid_location` | `location` is not a valid string/integer |
| 400 | `invalid_depth` | `depth` not an integer in range 1-200 |
| 400 | `invalid_device` | `device` not `"desktop"` or `"mobile"` |
| 400 | `invalid_language` | `language` is empty or not a string |
| 400 | `invalid_ai_optimized` | `ai_optimized` present but not a boolean |
| 401 | `unauthorized` | `csvkey` does not match the secret |
| 404 | `not_found` | Unrecognized path |
| 405 | `method_not_allowed` | Wrong HTTP method for the path |
| 429 | `dataforseo_rate_limited` | DataForSEO's rate limits (`40202`, `40209`) still turned the task request away after a minute of resending. No task was set and nothing was paid: retry later. The response has `Retry-After: 60` and DataForSEO's last answer in `dataforseo_body` |
| 500 | `missing_config` | Server secret not configured |
| 502 | `dataforseo_error` | DataForSEO returned non-2xx or fetch failed. For a result request, three times in a row, where a DataForSEO server error (`status_code` 50000 or higher) also counts as a failure |
| 504 | `dataforseo_timeout` | The Worker did not get the finished task within 280 seconds, because it was still running or DataForSEO's rate limits kept turning result requests away (the body adds `dataforseo_task_id`), or a request to DataForSEO timed out |

**DataForSEO errors** include additional context:

```json
{
  "error": "DataForSEO request failed",
  "code": "dataforseo_error",
  "dataforseo_status": 401,
  "dataforseo_body": "...(truncated to 4KB, UTF-8 safe)..."
}
```

A `dataforseo_status` of `0` means the request never reached DataForSEO (DNS failure, network error, etc.).

## Agent Integration

### LangChain / LangGraph

```python
from langchain_core.tools import tool
from pydantic import BaseModel, Field

class SerpInput(BaseModel):
    keyword: str = Field(description="The Google search query to execute")
    location: int | str = Field(description="DataForSEO location code (int) or name (str)")
    depth: int = Field(default=10, description="Number of results (1-200)")
    device: str = Field(default="desktop", description="'desktop' or 'mobile'")
    language: str = Field(default="en", description="Language code")
    ai_optimized: bool = Field(default=False, description="Use AI-optimized response format")

@tool(args_schema=SerpInput)
def fetch_serp(keyword: str, location: int | str, depth: int = 10,
               device: str = "desktop", language: str = "en",
               ai_optimized: bool = False) -> dict:
    """Fetch live Google SERP results from DataForSEO via rusty-serp."""
    import httpx
    response = httpx.post(
        f"https://rusty-serp.example.com/v1/serp?csvkey={CSVKEY}",
        json={
            "keyword": keyword, "location": location, "depth": depth,
            "device": device, "language": language, "ai_optimized": ai_optimized,
        },
        timeout=300,
    )
    response.raise_for_status()
    return response.json()
```

### Deep Agents

```python
from deep_agents import Tool

serp_tool = Tool(
    name="fetch_serp",
    description="Fetch live Google SERP results",
    endpoint="https://rusty-serp.example.com/v1/serp",
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

### Best Practices

- **Use `ai_optimized: true` by default** when agents consume SERP data for analysis. The flat structure with stripped null fields reduces token usage significantly.
- **Use `ai_optimized: false`** when agents need full metadata (rectangles, XPath, position data, cost breakdown).
- **Set `depth` conservatively.** Start with `10` (default). Each additional 10 results adds ~$0.0012.
- **Check `status_code` in the response body.** A `200` HTTP response does not guarantee SERP success; look for `20000` in the JSON.
- **Set agent tool timeout to 300 seconds.** Redirect resolution and the poisoned-result check are always on. DataForSEO caps redirect resolution at 260 seconds in total and gives no time figure for the check; in testing, requests took 145 to 186 seconds. The Worker gives up at 280 seconds and answers `504`, and abandons any result request after 10 seconds, so it answers within about 290 seconds and a 300-second client timeout always gets the Worker's answer.
- **Retry a `429` later.** It means DataForSEO's rate limits outlasted the Worker's minute of resending; nothing was paid, so trying again after a minute or more is safe.
- **Handle non-JSON errors.** If the Worker throws an exception or the connection drops, agents receive a Cloudflare HTML error page or a connection reset, not a JSON response.

## Common Location Codes

| Location | Code | Name |
|----------|------|------|
| United States | `2840` | `"United States"` |
| United Kingdom | `2826` | `"United Kingdom"` |
| Canada | `2124` | `"Canada"` |
| Australia | `2036` | `"Australia"` |
| Germany | `2276` | `"Germany"` |
| France | `2250` | `"France"` |
| India | `2356` | `"India"` |
| New York, US | `1023191` | `"New York,New York,United States"` |
| London, UK | `1006886` | `"London,England,United Kingdom"` |
| Los Angeles, US | `1013962` | `"Los Angeles,California,United States"` |

Full list: `GET https://api.dataforseo.com/v3/serp/google/locations`

## Local Development

```bash
# 1. Create .dev.vars with your credentials (gitignored)
cat > .dev.vars << 'EOF'
CSVKEY=your-local-test-key
DATAFORSEO_LOGIN=your-email@example.com
DATAFORSEO_PASSWORD=your-api-password
EOF

# 2. Start the local dev server
wrangler dev

# 3. Test the health endpoint
curl http://localhost:8787/v1/health

# 4. Test a SERP query
curl -X POST "http://localhost:8787/v1/serp?csvkey=your-local-test-key" \
  -H "Content-Type: application/json" \
  -d '{"keyword": "rust programming", "location": 2840}'
```

Use a current Wrangler (`npm install -g wrangler` updates it). The `worker` crate relies on the runtime's `tracing.getActiveSpan` and `tracing.withActiveSpan` for every request. Older Wrangler releases bundle a runtime without them, and then every request fails with HTTP 500 and `Uncaught Error: Critical error` in the log. Wrangler 4.167.0 works; 4.96.0 does not.

## Deployment

Production deploys automatically. The repository is connected to the `rusty-serp` Worker through Cloudflare Workers Builds, so every push to `main` builds the Worker with the `[build]` command in `wrangler.toml` and deploys it to production. The result shows up as the **Workers Builds: rusty-serp** check on the commit in GitHub.

The top-level `name` in `wrangler.toml` is `rusty-serp`, the production Worker, because Workers Builds fails the build unless it matches the connected Worker. So a bare `wrangler deploy` from your machine deploys to **production** too. Use `--env staging` for anything that must not reach production.

```bash
# Set production secrets (once, entered interactively)
wrangler secret put CSVKEY
wrangler secret put DATAFORSEO_LOGIN
wrangler secret put DATAFORSEO_PASSWORD

# Deploy to staging (set its secrets the same way, with --env staging)
wrangler deploy --env staging

# Deploy to production by hand (a push to main normally does this)
wrangler deploy
```

### Workers Builds Setup (Cloudflare Dashboard)

This is how production deployment is set up. Repeat it only to connect a new Worker to the repository.

1. Log in to the [Cloudflare dashboard](https://dash.cloudflare.com/) and navigate to **Workers & Pages**.
2. Click **Create** and select **Import a repository**.
3. Connect your GitHub account if you haven't already, then select the `rusty-serp` repository.
4. Configure the build settings:
   - **Build command:** leave empty (`wrangler.toml` defines the build command, which includes installing Rust if needed)
   - **Deploy command:** `npx wrangler deploy`
   - **Root directory:** leave empty (the project root contains `Cargo.toml` and `wrangler.toml`)
5. Click **Deploy**. Cloudflare will clone the repo, compile the Rust source to WebAssembly, and deploy the Worker.
6. After the first deploy completes, go to **Settings > Variables and Secrets** for the Worker and add the three required secrets:
   - `CSVKEY`
   - `DATAFORSEO_LOGIN`
   - `DATAFORSEO_PASSWORD`

   Click **Encrypt** for each value to store them as encrypted secrets.

With this setup, every push to the production branch (`main`) triggers an automatic rebuild and deploy. The production branch is set under **Settings > Build > Branch control**. To keep building without deploying, change the deploy command to `npx wrangler versions upload`, which only uploads a new version. To stop builds altogether, disconnect the repository under **Settings > Build**.

To roll back a deployment, navigate to **Workers & Pages > rusty-serp > Deployments**, find the version you want, and click **Rollback**.

### Environment Names

| How | Worker Name | Purpose |
|-----|-------------|---------|
| Push to `main` (Workers Builds) | `rusty-serp` | Production |
| `wrangler deploy` | `rusty-serp` | Production |
| `wrangler deploy --env production` | `rusty-serp` | Production (same Worker) |
| `wrangler deploy --env staging` | `rusty-serp-staging` | Pre-production testing |
| `wrangler dev` | none (local) | Local development |

## Cost Reference

rusty-serp uses DataForSEO's Standard method at high priority (`priority: 2`), which is charged when the task is set. Fetching the result is free.

| Scenario | DataForSEO Cost |
|----------|----------------|
| Basic query, 10 results | **$0.003** ($0.0012 base + $0.0006 AI overview + $0.0012 poisoned-result check) |
| Basic query, 20 results | **$0.0042** ($0.0024 for 2 pages + $0.0006 AI overview + $0.0012 poisoned-result check) |
| AI overview absent/cached | AI overview surcharge refunded |
| Query with search operators | Base cost x5 |
| `.ai` format (AI-optimized) | No additional cost |
| Redirect resolution (`resolve_goto_urls`, always on) | No extra charge listed by DataForSEO |
| Poisoned-result check (`resolve_poisoned_results`, always on) | Price of one more SERP page ($0.0012 at high priority) per request, whatever the depth and whether or not anything was poisoned |

In testing, a 10-result query cost $0.0024 with the AI overview surcharge refunded: two pages at $0.0012, one of them for the poisoned-result check.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `worker` | 0.9 | Cloudflare Workers runtime bindings |
| `serde` | 1 | JSON deserialization with derive macros |
| `serde_json` | 1 | JSON parsing and value construction |
| `base64` | 0.22 | DataForSEO Basic Auth encoding |
| `console_error_panic_hook` | 0.1 | WASM-friendly panic diagnostics |

## License

See [LICENSE](LICENSE).
