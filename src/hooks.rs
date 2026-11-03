//! Server-side hooks and webhooks.
//!
//! Hooks are executables, as in Git. `pre-receive` runs once a push's pack has
//! arrived and before any ref moves; a non-zero exit rejects the whole push (use it
//! for branch protection or policy checks). `post-receive` runs after refs moved. Both
//! read `<old> <new> <ref>` lines on stdin, and see `PANDO_REPO`, `PANDO_USER`,
//! `PANDO_PACK` (the pushed pack, when there is one) and Git's `GIT_PUSH_OPTION_COUNT`
//! and `GIT_PUSH_OPTION_<n>`. Whatever they print is shown to the pusher. A
//! repository's own `<data>/repos/<repo>.git/hooks/<name>` wins over the server-wide
//! `<data>/hooks/<name>`.
//!
//! Webhooks: after a push moves refs, each `--webhook` URL gets a POST with a JSON
//! body `{"repository", "pusher", "updates": [{"ref", "before", "after"}]}`. With a
//! secret, `X-Pando-Signature-256: sha256=<hex>` carries an HMAC-SHA256 of the body
//! (the scheme GitHub's `X-Hub-Signature-256` uses). Failed deliveries are retried.

use crate::object::Oid;
use crate::server::{Repo, Server};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct Update<'a> {
    pub old: Oid,
    pub new: Oid,
    pub refname: &'a str,
}

/// What a hook can know about a push.
pub struct Push<'a> {
    pub server: &'a Server,
    pub repo: &'a Repo,
    pub user: Option<&'a str>,
    pub options: &'a [String],
    pub pack: Option<PathBuf>,
}

/// The hook executable for `name`, if one is installed.
fn find(push: &Push, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    [
        push.repo.dir.join("hooks").join(name),
        push.server.cfg.data.join("hooks").join(name),
    ]
    .into_iter()
    .find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Run hook `name`, if installed. Returns whether it succeeded and what it printed.
pub fn run(push: &Push, name: &str, updates: &[Update]) -> anyhow::Result<Option<(bool, Vec<u8>)>> {
    let Some(path) = find(push, name) else {
        return Ok(None);
    };
    let mut cmd = Command::new(&path);
    cmd.current_dir(&push.repo.dir)
        .env("PANDO_REPO", &push.repo.name)
        .env("PANDO_USER", push.user.unwrap_or(""))
        .env("GIT_PUSH_OPTION_COUNT", push.options.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pack) = &push.pack {
        cmd.env("PANDO_PACK", pack);
    }
    for (i, o) in push.options.iter().enumerate() {
        cmd.env(format!("GIT_PUSH_OPTION_{i}"), o);
    }
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input: String = updates
        .iter()
        .map(|u| format!("{} {} {}\n", u.old, u.new, u.refname))
        .collect();
    // A hook may exit without reading its input.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let mut printed = out.stdout;
    printed.extend(out.stderr);
    tracing::info!(repo = %push.repo.name, hook = name, ok = out.status.success(), "hook ran");
    Ok(Some((out.status.success(), printed)))
}

/// Send the webhooks for refs that moved. Deliveries run in the background.
pub fn notify(server: &Server, repo: &str, user: Option<&str>, updates: &[Update]) {
    if server.cfg.webhooks.is_empty() || updates.is_empty() {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let body = payload(repo, user, updates);
    let signature = server.cfg.webhook_secret.as_ref().map(|secret| {
        format!(
            "sha256={}",
            hex::encode(crate::auth::hmac_sha256(secret.as_bytes(), body.as_bytes()))
        )
    });
    for url in &server.cfg.webhooks {
        let (url, body, signature) = (url.clone(), body.clone(), signature.clone());
        let client = server.http.clone();
        rt.spawn(async move {
            for attempt in 0..5u32 {
                let mut req = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("X-Pando-Event", "push")
                    .timeout(Duration::from_secs(10))
                    .body(body.clone());
                if let Some(sig) = &signature {
                    req = req.header("X-Pando-Signature-256", sig);
                }
                match req.send().await.and_then(|r| r.error_for_status()) {
                    Ok(_) => return,
                    Err(e) => tracing::warn!(url, attempt, "webhook failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(1 << (2 * attempt))).await;
            }
            crate::metrics::WEBHOOK_FAILURES.inc();
        });
    }
}

fn payload(repo: &str, user: Option<&str>, updates: &[Update]) -> String {
    let updates: Vec<String> = updates
        .iter()
        .map(|u| {
            format!(
                r#"{{"ref":{},"before":"{}","after":"{}"}}"#,
                json_string(u.refname),
                u.old,
                u.new
            )
        })
        .collect();
    format!(
        r#"{{"repository":{},"pusher":{},"updates":[{}]}}"#,
        json_string(repo),
        user.map_or("null".to_string(), json_string),
        updates.join(",")
    )
}

pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_json() {
        let u = [Update {
            old: Oid::ZERO,
            new: Oid([1; 20]),
            refname: "refs/heads/a\"b",
        }];
        assert_eq!(
            payload("team/app", None, &u),
            format!(
                r#"{{"repository":"team/app","pusher":null,"updates":[{{"ref":"refs/heads/a\"b","before":"{}","after":"{}"}}]}}"#,
                Oid::ZERO,
                Oid([1; 20])
            )
        );
    }
}
