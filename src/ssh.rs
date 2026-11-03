//! Git over SSH: `git clone ssh://git@host:2222/<repo>.git`.
//!
//! Clients authenticate with a public key from the auth file (any key when auth is
//! off). The only commands are `git-upload-pack` and `git-receive-pack`, which speak
//! the same protocols as smart HTTP over the channel; `GIT_PROTOCOL` (which git sends
//! with `SendEnv`) selects protocol v2. The host key is created on first start in
//! `<data>/ssh_host_ed25519_key`.

use crate::auth::Access;
use crate::server::{Server, valid_repo_name};
use crate::{receive, upload};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Msg, PublicKeyOffer, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::io::SyncIoBridge;

/// Serve SSH on `listener` until the returned future is dropped.
pub async fn serve(server: Arc<Server>, listener: TcpListener) -> anyhow::Result<()> {
    let key = host_key(&server.cfg.data.join("ssh_host_ed25519_key"))?;
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(600)),
        nodelay: true,
        ..Default::default()
    });
    let mut front = Front { server };
    use russh::server::Server as _;
    front.run_on_socket(config, &listener).await?;
    Ok(())
}

/// Load the host key, creating it on first start.
fn host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        return Ok(PrivateKey::from_openssh(std::fs::read_to_string(path)?)?);
    }
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    let key = PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)),
        "pando host key",
    )?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(key.to_openssh(LineEnding::LF)?.as_bytes())?;
    tracing::info!("created SSH host key {}", path.display());
    Ok(key)
}

struct Front {
    server: Arc<Server>,
}

impl russh::server::Server for Front {
    type Handler = Conn;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Conn {
        Conn {
            server: self.server.clone(),
            key: None,
            protocol: None,
            channels: HashMap::new(),
        }
    }
}

/// One SSH connection.
struct Conn {
    server: Arc<Server>,
    /// The key the client authenticated with.
    key: Option<PublicKey>,
    /// `GIT_PROTOCOL` from the client's environment.
    protocol: Option<String>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl Conn {
    fn known(&self, key: &PublicKey) -> bool {
        self.server
            .cfg
            .auth
            .as_ref()
            .is_none_or(|a| a.check_key(key).is_some())
    }
}

impl russh::server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.known(offer.public_key) {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if !self.known(key.public_key) {
            return Ok(Auth::reject());
        }
        self.key = Some(key.public_key.clone());
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" {
            self.protocol = Some(value.to_string());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        session.channel_success(id)?;
        let handle = session.handle();
        let server = self.server.clone();
        let key = self.key.clone();
        let protocol = self.protocol.clone();
        let command = String::from_utf8_lossy(data).into_owned();
        tokio::spawn(async move {
            let stderr = channel.make_writer_ext(Some(1));
            let (rd, wr) = tokio::io::split(channel.into_stream());
            let done = tokio::task::spawn_blocking(move || {
                let mut r = BufReader::with_capacity(64 << 10, SyncIoBridge::new(rd));
                let mut w = SyncIoBridge::new(wr);
                let mut err = SyncIoBridge::new(stderr);
                let result = run(
                    &server,
                    key.as_ref(),
                    protocol.as_deref(),
                    &command,
                    &mut r,
                    &mut w,
                );
                let _ = w.flush();
                let code = match result {
                    Ok(()) => 0,
                    Err(e) => {
                        tracing::warn!(command, "ssh: {e:#}");
                        let _ = writeln!(err, "fatal: {e:#}");
                        let _ = err.flush();
                        128
                    }
                };
                // The stream closes the channel when dropped, so it outlives the
                // exit status.
                (code, r, w, err)
            })
            .await;
            let code = done.as_ref().map_or(128, |d| d.0);
            let _ = handle.exit_status_request(id, code).await;
            let _ = handle.eof(id).await;
            let _ = handle.close(id).await;
            drop(done);
        });
        Ok(())
    }
}

/// Run one git command on a channel.
fn run(
    server: &Server,
    key: Option<&PublicKey>,
    protocol: Option<&str>,
    command: &str,
    r: &mut dyn std::io::BufRead,
    w: &mut dyn Write,
) -> anyhow::Result<()> {
    crate::metrics::SSH_SESSIONS.inc();
    let (push, name) = parse_command(command)?;
    let need = if push { Access::Write } else { Access::Read };
    let user = match (&server.cfg.auth, key) {
        (None, _) => None,
        (Some(auth), Some(key)) => {
            let id = auth
                .check_key(key)
                .ok_or_else(|| anyhow::anyhow!("unknown key"))?;
            if !id.allows(&name, need) {
                let what = if push { "push to" } else { "read" };
                anyhow::bail!("{} cannot {what} {name}", id.name);
            }
            Some(id.name.clone())
        }
        (Some(_), None) => anyhow::bail!("authentication required"),
    };
    let repo = server
        .open_repo(&name, push)?
        .ok_or_else(|| anyhow::anyhow!("repository '{name}' not found"))?;
    tracing::info!(repo = name, push, user, "ssh");
    if push {
        receive::serve_stream(server, &repo, user.as_deref(), r, w)
    } else {
        let version = upload::Version::from_git_protocol(protocol);
        upload::serve_stream(server, &repo, version, r, w)
    }
}

/// `git-upload-pack '/team/app.git'` (also `git upload-pack`, with or without the
/// leading slash, `~/`, quotes, or `.git`) into (is a push, repository name).
fn parse_command(command: &str) -> anyhow::Result<(bool, String)> {
    let command = command.trim();
    let (prog, arg) = command
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("unsupported command {command:?}"))?;
    let (prog, arg) = match prog {
        "git" => {
            let (sub, arg) = arg.trim_start().split_once(' ').unwrap_or((arg, ""));
            (format!("git-{sub}"), arg)
        }
        _ => (prog.to_string(), arg),
    };
    let push = match prog.as_str() {
        "git-upload-pack" => false,
        "git-receive-pack" => true,
        _ => anyhow::bail!("only git-upload-pack and git-receive-pack are supported"),
    };
    let path = arg.trim().trim_matches(|c| c == '\'' || c == '"');
    let path = path.strip_prefix("~/").unwrap_or(path);
    let path = path.trim_matches('/');
    let name = path.strip_suffix(".git").unwrap_or(path);
    if !valid_repo_name(name) {
        anyhow::bail!("invalid repository name {path:?}");
    }
    Ok((push, name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::parse_command;

    #[test]
    fn commands_parse_like_git_sends_them() {
        for (cmd, want) in [
            ("git-upload-pack '/team/app.git'", (false, "team/app")),
            ("git-upload-pack 'app.git'", (false, "app")),
            ("git receive-pack '~/app.git'", (true, "app")),
            ("git-receive-pack '/app.git/'", (true, "app")),
            ("git-upload-pack app", (false, "app")),
        ] {
            let (push, name) = parse_command(cmd).unwrap();
            assert_eq!((push, name.as_str()), want, "{cmd}");
        }
        assert!(parse_command("rm -rf /").is_err());
        assert!(parse_command("git-upload-pack '../etc.git'").is_err());
        assert!(parse_command("git-upload-archive 'app.git'").is_err());
    }
}
