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

/// Signs pack URLs handed out in fetch responses, so a client can download exactly the
/// packs it was authorized for, for a limited time, without credentials. Express nodes
/// given the same secret check the same signatures.
pub struct UrlSigner {
    secret: Vec<u8>,
}

/// How long a signed pack URL stays valid.
const URL_TTL_SECS: u64 = 3600;

impl UrlSigner {
    pub fn new(secret: &[u8]) -> UrlSigner {
        UrlSigner {
            secret: secret.to_vec(),
        }
    }

    /// A signer with a random secret (URLs stop working when the process restarts).
    pub fn random() -> std::io::Result<UrlSigner> {
        use std::io::Read;
        let mut secret = vec![0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut secret)?;
        Ok(UrlSigner { secret })
    }

    /// The query string (`exp=...&sig=...`) authorizing `repo`'s pack `hash`.
    pub fn sign(&self, repo: &str, hash: &crate::object::Oid) -> String {
        let exp = now() + URL_TTL_SECS;
        format!("exp={exp}&sig={}", hex::encode(self.mac(repo, hash, exp)))
    }

    pub fn verify(&self, repo: &str, hash: &crate::object::Oid, query: &str) -> bool {
        let field = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k));
        let (Some(exp), Some(sig)) = (field("exp="), field("sig=")) else {
            return false;
        };
        let (Ok(exp), Ok(sig)) = (exp.parse::<u64>(), hex::decode(sig)) else {
            return false;
        };
        exp >= now() && constant_time_eq(&self.mac(repo, hash, exp), &sig)
    }

    fn mac(&self, repo: &str, hash: &crate::object::Oid, exp: u64) -> [u8; 20] {
        hmac_sha1(&self.secret, format!("{repo}\n{hash}\n{exp}").as_bytes())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// HMAC (RFC 2104) over SHA-1.
fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = Sha1::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha1::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_2202() {
        let mac = hmac_sha1(&[0x0b; 20], b"Hi There");
        assert_eq!(hex::encode(mac), "b617318655057264e28bc0b6fb378c8ef146be00");
    }

    #[test]
    fn signed_urls_verify_only_for_their_pack() {
        let s = UrlSigner::new(b"k");
        let h = crate::object::Oid([7; 20]);
        let q = s.sign("r", &h);
        assert!(s.verify("r", &h, &q));
        assert!(!s.verify("other", &h, &q));
        assert!(!s.verify("r", &crate::object::Oid([8; 20]), &q));
        assert!(!UrlSigner::new(b"x").verify("r", &h, &q));
    }
}
