//! Who may do what: tokens over HTTP Basic (what git's credential helpers speak) or
//! Bearer, and public keys over SSH.
//!
//! The auth file has one entry per line:
//!
//! ```text
//! # name   credential              access  [repositories]
//! ishaan   tok_3f9a...             admin
//! ishaan   ssh-ed25519 AAAAC3...   write
//! ci       tok_77c1...             read    team/*,docs
//! ```
//!
//! The credential is a token or an OpenSSH public key (a pasted `.pub` line, comment
//! included, works). Access is `read`, `write` (push,
//! including creating repositories), or `admin` (also the admin API). The optional last
//! field limits the entry to repositories matching comma-separated patterns, where `*`
//! matches any run of characters. With HTTP Basic, the token goes in the password field
//! and the username is ignored.

use axum::http::{HeaderMap, header};
use base64::Engine;
use russh::keys::PublicKey;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Read,
    Write,
    Admin,
}

enum Credential {
    Token(String),
    Key(Box<PublicKey>),
}

/// One entry of the auth file.
pub struct Identity {
    pub name: String,
    pub access: Access,
    /// Repository patterns; empty means every repository.
    repos: Vec<String>,
    credential: Credential,
}

impl Identity {
    /// Whether this identity may act on `repo` with at least `need`.
    pub fn allows(&self, repo: &str, need: Access) -> bool {
        self.access >= need && self.can_see(repo)
    }

    /// Whether `repo` is within this identity's repositories at all.
    pub fn can_see(&self, repo: &str) -> bool {
        self.repos.is_empty() || self.repos.iter().any(|p| glob(p, repo))
    }
}

/// `*` matches any run of characters; everything else matches itself.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, rest)) => {
            let Some(name) = name.strip_prefix(head) else {
                return false;
            };
            (0..=name.len())
                .filter(|&i| name.is_char_boundary(i))
                .any(|i| glob(rest, &name[i..]))
        }
    }
}

pub struct Auth {
    identities: Vec<Identity>,
}

impl Auth {
    pub fn load(path: &Path) -> anyhow::Result<Auth> {
        Auth::parse(&std::fs::read_to_string(path)?)
            .map_err(|(n, e)| anyhow::anyhow!("{}:{n}: {e}", path.display()))
    }

    /// Parse auth file contents. Errors carry the 1-based line number.
    pub fn parse(text: &str) -> Result<Auth, (usize, String)> {
        let mut identities = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let fail = |e: String| (n + 1, e);
            let is_key = fields
                .get(1)
                .is_some_and(|f| ["ssh-", "ecdsa-", "sk-"].iter().any(|p| f.starts_with(p)));
            let (credential, rest) = if is_key {
                let [_, kind, data, rest @ ..] = &fields[..] else {
                    return Err(fail("expected `<name> <key-type> <key> <access>`".into()));
                };
                let key = PublicKey::from_openssh(&format!("{kind} {data}"))
                    .map_err(|e| fail(format!("bad public key: {e}")))?;
                // A pasted .pub line may carry a comment before the access level.
                let access_at = rest
                    .iter()
                    .rposition(|f| ["read", "write", "admin"].contains(f))
                    .filter(|&i| i + 2 >= rest.len())
                    .unwrap_or(0);
                (Credential::Key(Box::new(key)), &rest[access_at..])
            } else {
                let [_, token, rest @ ..] = &fields[..] else {
                    return Err(fail("expected `<name> <token> <access>`".into()));
                };
                (Credential::Token(token.to_string()), rest)
            };
            let (access, repos) = match rest {
                [access] => (*access, ""),
                [access, repos] => (*access, *repos),
                _ => {
                    return Err(fail(
                        "expected `<access> [repositories]` after the credential".into(),
                    ));
                }
            };
            let access = match access {
                "read" => Access::Read,
                "write" => Access::Write,
                "admin" => Access::Admin,
                other => return Err(fail(format!("unknown access {other:?}"))),
            };
            identities.push(Identity {
                name: fields[0].into(),
                access,
                repos: repos
                    .split(',')
                    .filter(|p| !p.is_empty())
                    .map(String::from)
                    .collect(),
                credential,
            });
        }
        Ok(Auth { identities })
    }

    /// The identity behind a request's token, if it carries a known one.
    pub fn check(&self, headers: &HeaderMap) -> Option<&Identity> {
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
        self.identities.iter().find(|i| match &i.credential {
            Credential::Token(t) => constant_time_eq(t.as_bytes(), secret.as_bytes()),
            Credential::Key(_) => false,
        })
    }

    /// The identity a public key belongs to.
    pub fn check_key(&self, key: &PublicKey) -> Option<&Identity> {
        self.identities.iter().find(|i| match &i.credential {
            Credential::Key(k) => k.key_data() == key.key_data(),
            Credential::Token(_) => false,
        })
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

/// HMAC (RFC 2104) over SHA-256.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
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
    fn auth_file_entries_scope_access() {
        let auth = Auth::parse(
            "# comment\n\
             me tok1 admin\n\
             me ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGpBGs2XPDlcI6sJYkTyLe5bfHgGoN0VD7O4OVgoLbmZ laptop write\n\
             ci tok2 read team/*,docs\n",
        )
        .unwrap();
        let ci = &auth.identities[2];
        assert!(ci.allows("team/app", Access::Read));
        assert!(ci.allows("docs", Access::Read));
        assert!(!ci.allows("team/app", Access::Write));
        assert!(!ci.allows("other", Access::Read));
        assert!(!ci.allows("docs2", Access::Read));
        assert_eq!(auth.identities[1].access, Access::Write);
        assert!(auth.identities[0].allows("anything/at/all", Access::Admin));
        assert!(Auth::parse("me tok1 owner\n").is_err());
        assert!(Auth::parse("me ssh-ed25519 notbase64 write\n").is_err());
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex::encode(mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

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
