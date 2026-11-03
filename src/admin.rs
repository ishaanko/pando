//! Administration over HTTP, for auth entries with `admin` access (on the repository
//! concerned; listing shows the repositories an admin can see):
//!
//! - `GET    /api/repos`                          list repositories (JSON)
//! - `DELETE /api/repos/<repo>`                   delete a repository
//! - `POST   /api/repos/<repo>/rename?to=<name>`  rename a repository
//! - `PUT    /api/repos/<repo>/head?branch=<b>`   change the default branch
//! - `POST   /api/repos/<repo>/gc`                collect garbage now
//!
//! For example: `curl -X DELETE -H "Authorization: Bearer $TOKEN" $URL/api/repos/old`.

use crate::auth::Access;
use crate::hooks::json_string;
use crate::server::{Server, blocking};
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub async fn handle(server: Arc<Server>, req: Request) -> Response {
    let Some(auth) = &server.cfg.auth else {
        return (
            StatusCode::FORBIDDEN,
            "the admin API needs an auth file with an admin entry\n",
        )
            .into_response();
    };
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let param = |key: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .map(|v| v.replace("%2F", "/").replace("%2f", "/"))
    };
    let Some(rest) = path.strip_prefix("/api/repos") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let method = req.method().clone();

    if rest.is_empty() || rest == "/" {
        if method != Method::GET {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let Some(id) = auth
            .check(req.headers())
            .filter(|i| i.access == Access::Admin)
        else {
            return denied();
        };
        let visible: Vec<String> = match server.list_repos() {
            Ok(names) => names.into_iter().filter(|n| id.can_see(n)).collect(),
            Err(e) => return error(e),
        };
        let s = server.clone();
        return match blocking(move || list_json(&s, &visible)).await {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => error(e),
        };
    }

    let rest = rest.trim_start_matches('/');
    let (name, action) = ["/rename", "/head", "/gc"]
        .iter()
        .find_map(|a| rest.strip_suffix(a).map(|n| (n, &a[1..])))
        .unwrap_or((rest, ""));
    let name = name.trim_end_matches(".git").to_string();
    if server
        .authorize(req.headers(), &name, Access::Admin)
        .is_err()
    {
        return denied();
    }
    let s = server.clone();
    let result = match (method, action) {
        (Method::DELETE, "") => blocking(move || Ok(s.delete_repo(&name)?.then(String::new))).await,
        (Method::POST, "rename") => {
            let Some(to) = param("to") else {
                return (StatusCode::BAD_REQUEST, "missing ?to=\n").into_response();
            };
            blocking(move || Ok(s.rename_repo(&name, &to)?.then(String::new))).await
        }
        (Method::PUT, "head") => {
            let Some(branch) = param("branch") else {
                return (StatusCode::BAD_REQUEST, "missing ?branch=\n").into_response();
            };
            let branch = if branch.starts_with("refs/") {
                branch
            } else {
                format!("refs/heads/{branch}")
            };
            blocking(move || {
                let Some(repo) = s.repo(&name)? else {
                    return Ok(None);
                };
                let mut meta = repo.meta.lock().unwrap();
                meta.set_trunk(&branch)?.map_err(anyhow::Error::msg)?;
                tracing::info!(repo = name, branch, "default branch changed");
                Ok(Some(String::new()))
            })
            .await
        }
        (Method::POST, "gc") => {
            let grace = s.cfg.gc_grace;
            blocking(move || {
                let Some(repo) = s.repo(&name)? else {
                    return Ok(None);
                };
                let r = crate::gc::run(&repo, grace)?;
                Ok(Some(format!(
                    r#"{{"live":{},"marked":{},"deleted":{},"bytes":{}}}"#,
                    r.live, r.marked, r.deleted, r.bytes
                )))
            })
            .await
        }
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    match result {
        Ok(Some(body)) if body.is_empty() => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(body)) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "repository not found\n").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}\n")).into_response(),
    }
}

fn list_json(server: &Server, names: &[String]) -> anyhow::Result<String> {
    let mut items = Vec::new();
    for name in names {
        let Some(repo) = server.repo(name)? else {
            continue;
        };
        let meta = repo.meta.lock().unwrap();
        let refs = meta.refs(&[])?.len();
        items.push(format!(
            r#"{{"name":{},"head":{},"refs":{refs}}}"#,
            json_string(name),
            json_string(&meta.trunk)
        ));
    }
    Ok(format!("[{}]", items.join(",")))
}

fn denied() -> Response {
    (StatusCode::FORBIDDEN, "admin access required\n").into_response()
}

fn error(e: anyhow::Error) -> Response {
    tracing::error!("admin: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response()
}
