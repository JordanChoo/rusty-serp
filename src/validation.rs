use crate::errors::json_error;
use worker::*;

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

pub fn validate_auth(url: &Url, env: &Env) -> std::result::Result<(), Response> {
    let csvkey = url
        .query_pairs()
        .find(|(key, _)| key == "csvkey")
        .map(|(_, value)| value.to_string())
        .ok_or_else(|| {
            json_error(400, "Missing csvkey query parameter", "missing_csvkey").unwrap()
        })?;

    let secret = env
        .secret("CSVKEY")
        .map(|s| s.to_string())
        .map_err(|_| json_error(500, "CSVKEY not configured", "missing_config").unwrap())?;

    if !constant_time_eq(csvkey.as_bytes(), secret.as_bytes()) {
        return Err(json_error(401, "Invalid csvkey", "unauthorized").unwrap());
    }

    Ok(())
}

pub async fn parse_and_validate_body(
    req: &mut Request,
) -> std::result::Result<SerpRequest, Response> {
    let body_text = req.text().await.map_err(|_| {
        json_error(
            400,
            "Failed to read request body (body may be missing, empty, or not valid UTF-8)",
            "missing_body",
        )
        .unwrap()
    })?;

    if body_text.trim().is_empty() {
        return Err(json_error(400, "Request body is empty", "missing_body").unwrap());
    }

    let body: serde_json::Value = serde_json::from_str(&body_text)
        .map_err(|_| json_error(400, "Invalid JSON in request body", "invalid_json").unwrap())?;

    let keyword = body
        .get("keyword")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            json_error(400, "Missing or empty 'keyword' field", "missing_keyword").unwrap()
        })?;

    if keyword.len() > 700 {
        return Err(
            json_error(400, "Keyword exceeds 700 character limit", "invalid_keyword").unwrap(),
        );
    }

    let location = match body.get("location") {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .filter(|&code| code > 0)
            .map(Location::Code)
            .ok_or_else(|| {
                json_error(
                    400,
                    "Location code must be a positive integer",
                    "invalid_location",
                )
                .unwrap()
            })?,
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                return Err(
                    json_error(400, "Location name must not be empty", "invalid_location").unwrap(),
                );
            }
            Location::Name(trimmed)
        }
        _ => {
            return Err(
                json_error(400, "Missing or invalid 'location' field", "missing_location").unwrap(),
            )
        }
    };

    let depth = match body.get("depth") {
        Some(v) => v
            .as_u64()
            .and_then(|d| u32::try_from(d).ok())
            .filter(|&d| d >= 1 && d <= 700)
            .ok_or_else(|| {
                json_error(
                    400,
                    "Depth must be an integer between 1 and 700",
                    "invalid_depth",
                )
                .unwrap()
            })?,
        None => 10,
    };

    let device = match body.get("device") {
        Some(v) => v
            .as_str()
            .map(|s| s.to_lowercase())
            .filter(|s| s == "desktop" || s == "mobile")
            .ok_or_else(|| {
                json_error(400, "Device must be 'desktop' or 'mobile'", "invalid_device").unwrap()
            })?,
        None => "desktop".to_string(),
    };

    let language = match body.get("language") {
        Some(v) => v
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                json_error(400, "Language must be a non-empty string", "invalid_language").unwrap()
            })?,
        None => "en".to_string(),
    };

    let ai_optimized = match body.get("ai_optimized") {
        Some(v) => v.as_bool().ok_or_else(|| {
            json_error(
                400,
                "ai_optimized must be a boolean (true or false)",
                "invalid_ai_optimized",
            )
            .unwrap()
        })?,
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

pub fn read_secret(env: &Env, name: &str) -> std::result::Result<String, Response> {
    env.secret(name)
        .map(|s| s.to_string())
        .map_err(|_| json_error(500, &format!("{} not configured", name), "missing_config").unwrap())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
