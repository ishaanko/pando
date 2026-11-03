//! Token authentication over HTTP Basic (what git's credential helpers speak) or Bearer.
//!
//! The token file has one entry per line: `<name> <token> <read|write>`. With HTTP
//! Basic, the token goes in the password field and the username is ignored.

use axum::http::{HeaderMap, header};
use base64::Engine;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Read,
    Write,
}

struct Token {
    name: String,
    secret: String,
    access: Access,
}

pub struct Auth {
    tokens: Vec<Token>,
}

impl Auth {
    pub fn load(path: &Path) -> anyhow::Result<Auth> {
        let mut tokens = Vec::new();
        for (n, line) in std::fs::read_to_string(path)?.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [name, secret, access] = fields[..] else {
                anyhow::bail!(
                    "{}:{}: expected `<name> <token> <read|write>`",
                    path.display(),
                    n + 1
                );
            };
            let access = match access {
                "read" => Access::Read,
                "write" => Access::Write,
                other => anyhow::bail!("{}:{}: unknown access {other:?}", path.display(), n + 1),
            };
            tokens.push(Token {
                name: name.into(),
                secret: secret.into(),
                access,
            });
        }
        Ok(Auth { tokens })
    }

    /// The caller's name and access, if the request carries a known token.
    pub fn check(&self, headers: &HeaderMap) -> Option<(&str, Access)> {
        let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
        let secret = if let Some(b64) = value.strip_prefix("Basic ") {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()?;
            let creds = String::from_utf8(decoded).ok()?;
            let (user, pass) = creds.split_once(':')?;
            if pass.is_empty() {
                user.to_string()
            } else {
                pass.to_string()
            }
        } else {
            value.strip_prefix("Bearer ")?.trim().to_string()
        };
        self.tokens
            .iter()
            .find(|t| constant_time_eq(t.secret.as_bytes(), secret.as_bytes()))
            .map(|t| (t.name.as_str(), t.access))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
