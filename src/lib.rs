use worker::*;

mod dataforseo;
mod errors;
mod validation;

use crate::dataforseo::fetch_serp;
use crate::errors::json_error;
use crate::validation::{parse_and_validate_body, read_secret, validate_auth};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let path = req.path();
    let method = req.method();

    match (method, path.as_str()) {
        (Method::Get, "/v1/health") => {
            let body = serde_json::json!({"status": "ok"});
            let mut resp = Response::ok(body.to_string())?;
            resp.headers_mut().set("Content-Type", "application/json")?;
            Ok(resp)
        }
        (Method::Head, "/v1/health") => {
            let mut resp = Response::empty()?.with_status(200);
            resp.headers_mut().set("Content-Type", "application/json")?;
            Ok(resp)
        }
        (Method::Post, "/v1/serp") => handle_serp(req, env).await,
        (_, "/v1/health") | (_, "/v1/serp") => {
            json_error(405, "Method not allowed", "method_not_allowed")
        }
        _ => json_error(404, "Not found", "not_found"),
    }
}

async fn handle_serp(mut req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;

    let result: std::result::Result<Response, Response> = async {
        validate_auth(&url, &env)?;
        let serp_request = parse_and_validate_body(&mut req).await?;
        let login = read_secret(&env, "DATAFORSEO_LOGIN")?;
        let password = read_secret(&env, "DATAFORSEO_PASSWORD")?;
        fetch_serp(&serp_request, &login, &password).await
    }
    .await;

    Ok(result.unwrap_or_else(|e| e))
}
