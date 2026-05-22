# rusty-serp

A Rust-based Cloudflare Worker that serves as a stateless HTTP proxy to the [DataForSEO](https://dataforseo.com/) SERP Google Organic Live Advanced API. It provides a unified, authenticated endpoint that Graph Agents call as a tool to retrieve live Google SERP data, including AI Overviews, organic results, People Also Ask, and related searches.

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
                                                                  | POST (Basic Auth)
                                                                  v
                                                    +----------------------------+
                                                    |        DataForSEO          |
                                                    | /v3/serp/google/organic/   |
                                                    |   live/advanced[.ai]       |
                                                    +----------------------------+
```

### Request Flow

1. Agent sends `POST /v1/serp?csvkey=<key>` with a JSON body
2. Worker validates the csvkey via constant-time comparison
3. Worker parses and validates the request body (keyword, location, depth, device, language, ai_optimized)
4. Worker constructs the DataForSEO request with Basic Auth and always sets `load_async_ai_overview: true`
5. Worker selects the standard or AI-optimized endpoint based on the `ai_optimized` flag
6. Worker returns the DataForSEO response verbatim, without transformation or filtering

### Module Structure

```
src/
  lib.rs           # Entry point, routing, orchestration
  validation.rs    # csvkey auth, JSON body parsing, field validation
  dataforseo.rs    # DataForSEO API client (request build, fetch, response handling)
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
| `depth` | integer | No | `10` | Number of results to retrieve (1-700). Charged per 10 by DataForSEO. |
| `device` | string | No | `"desktop"` | `"desktop"` or `"mobile"` (case-insensitive). |
| `language` | string | No | `"en"` | Language code (e.g., `"en"`, `"es"`, `"fr"`, `"de"`). |
| `ai_optimized` | boolean | No | `false` | When `true`, calls the `.ai` endpoint for a flattened, token-efficient response. Must be a JSON boolean; strings like `"true"` are rejected. |

**Location field behavior:** Type detection is based strictly on the JSON value type. An integer like `2840` maps to `location_code`. A string like `"United States"` maps to `location_name`. A string like `"2840"` is treated as a location *name*, not a code. The Worker does not attempt to parse strings as integers.

**Response:** `200 OK` with DataForSEO response body passed through verbatim. The HTTP status is always `200` for successful upstream calls; check the `status_code` field in the JSON body for DataForSEO-level success or failure (`20000` = success).

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
| 400 | `invalid_depth` | `depth` not an integer in range 1-700 |
| 400 | `invalid_device` | `device` not `"desktop"` or `"mobile"` |
| 400 | `invalid_language` | `language` is empty or not a string |
| 400 | `invalid_ai_optimized` | `ai_optimized` present but not a boolean |
| 401 | `unauthorized` | `csvkey` does not match the secret |
| 404 | `not_found` | Unrecognized path |
| 405 | `method_not_allowed` | Wrong HTTP method for the path |
| 500 | `missing_config` | Server secret not configured |
| 502 | `dataforseo_error` | DataForSEO returned non-2xx or fetch failed |
| 504 | `dataforseo_timeout` | Fetch-level timeout detected |

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
    depth: int = Field(default=10, description="Number of results (1-700)")
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
        timeout=120,
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
- **Set `depth` conservatively.** Start with `10` (default). Each additional 10 results adds ~$0.002.
- **Check `status_code` in the response body.** A `200` HTTP response does not guarantee SERP success; look for `20000` in the JSON.
- **Set agent tool timeout to 120+ seconds.** DataForSEO Live mode can take up to 120 seconds for complex queries.
- **Handle non-JSON timeouts.** If Cloudflare's wall-clock limit kills the Worker, agents receive a `524` HTML error page or connection reset, not a JSON response.

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

## Deployment

```bash
# Set secrets (once per environment, entered interactively)
wrangler secret put CSVKEY --env production
wrangler secret put DATAFORSEO_LOGIN --env production
wrangler secret put DATAFORSEO_PASSWORD --env production

# Deploy to production
wrangler deploy --env production

# Deploy to staging
wrangler deploy --env staging
```

A bare `wrangler deploy` (no `--env`) deploys to `rusty-serp-dev`, not production. This is by design; the top-level name in `wrangler.toml` differs from the production name to prevent accidental production deployments.

### Environment Names

| Command | Worker Name | Purpose |
|---------|-------------|---------|
| `wrangler deploy` | `rusty-serp-dev` | Development (safe default) |
| `wrangler deploy --env staging` | `rusty-serp-staging` | Pre-production testing |
| `wrangler deploy --env production` | `rusty-serp` | Production |

## Cost Reference

| Scenario | DataForSEO Cost |
|----------|----------------|
| Basic query, 10 results | **$0.004** ($0.002 base + $0.002 AI overview) |
| Basic query, 20 results | **$0.006** ($0.004 for 2 pages + $0.002) |
| AI overview absent/cached | AI overview surcharge refunded |
| Query with search operators | Base cost x5 |
| `.ai` endpoint (AI-optimized) | No additional cost |

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `worker` | 0.8 | Cloudflare Workers runtime bindings |
| `serde` | 1 | JSON deserialization with derive macros |
| `serde_json` | 1 | JSON parsing and value construction |
| `base64` | 0.22 | DataForSEO Basic Auth encoding |
| `console_error_panic_hook` | 0.1 | WASM-friendly panic diagnostics |

## License

See [LICENSE](LICENSE).
