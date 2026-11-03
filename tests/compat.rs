//! Client compatibility matrix: the same scenario, run by every Git client we can
//! find, over HTTP and SSH.
//!
//! Always run: the system `git` with protocol v0, v1 and v2. Run when their helper is
//! named in the environment (see `compat/setup.sh`): go-git (`PANDO_COMPAT_GOGIT`),
//! libgit2 (`PANDO_COMPAT_LIBGIT2`), JGit (`PANDO_COMPAT_JGIT`), gitoxide
//! (`PANDO_COMPAT_GIX`), and an old git binary (`PANDO_COMPAT_OLD_GIT`). With
//! `PANDO_COMPAT_REQUIRE=1`, a missing client is a failure.
//!
//! Helpers implement `clone <url> <dir>`, `fetch <dir>` and `push <dir> <refspec>`,
//! exiting 3 for operations the client does not support.

use pando::server::{Config, Server};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use tempfile::TempDir;

const TOKEN: &str = "compat-token";

/// How a client is driven.
enum Driver {
    /// A git binary with a protocol version.
    Git {
        bin: PathBuf,
        protocol: &'static str,
    },
    /// A helper implementing the contract above.
    Helper(PathBuf),
}

struct Client {
    name: String,
    driver: Driver,
}

enum Outcome {
    Ok,
    Unsupported,
}

struct Env {
    _dir: TempDir,
    root: PathBuf,
    http: String,
    ssh: String,
    key: PathBuf,
    ssh_command: String,
}

impl Env {
    fn start() -> Env {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let key = root.join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key)
                .status()
                .unwrap()
                .success()
        );
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let http = format!("http://{}", listener.local_addr().unwrap());
        let ssh_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ssh_listener.set_nonblocking(true).unwrap();
        let ssh = format!("ssh://git@{}", ssh_listener.local_addr().unwrap());
        let mut cfg = Config::new(root.join("data"), &http);
        cfg.compact_every = None;
        cfg.gc_every = None;
        cfg.auth = Some(
            pando::auth::Auth::parse(&format!("u {TOKEN} write\nu {} write\n", public.trim()))
                .unwrap(),
        );
        let server = Server::new(cfg).unwrap();
        let router = server.router();
        let ssh_server: Arc<Server> = server.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                let s = tokio::net::TcpListener::from_std(ssh_listener).unwrap();
                tokio::spawn(pando::ssh::serve(ssh_server, s));
                axum::serve(l, router).await.unwrap();
            });
        });
        let ssh_command = format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
             -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
            key.display()
        );
        Env {
            http: http.replace("http://", &format!("http://u:{TOKEN}@")),
            _dir: dir,
            root,
            ssh,
            key,
            ssh_command,
        }
    }

    /// The system git, as the reference client.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run(Path::new("git"), dir, &[], args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn run(&self, bin: &Path, dir: &Path, pre: &[&str], args: &[&str]) -> Output {
        Command::new(bin)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(pre)
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", &self.ssh_command)
            .output()
            .unwrap()
    }

    /// One client operation.
    fn op(&self, client: &Client, dir: &Path, op: &str, args: &[&str]) -> Result<Outcome, String> {
        let out = match &client.driver {
            Driver::Git { bin, protocol } => {
                let v = format!("protocol.version={protocol}");
                let pre = ["-c", v.as_str()];
                let full: Vec<&str> = match op {
                    "clone" => [&["clone", "-q"][..], args].concat(),
                    "fetch" => vec!["-C", args[0], "fetch", "-q", "origin"],
                    "push" => [&["-C", args[0], "push", "-q", "origin"][..], &args[1..]].concat(),
                    _ => unreachable!(),
                };
                self.run(bin, dir, &pre, &full)
            }
            Driver::Helper(bin) => Command::new(bin)
                .arg(op)
                .args(args)
                .current_dir(dir)
                .env("PANDO_SSH_KEY", &self.key)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap(),
        };
        match out.status.code() {
            Some(0) => Ok(Outcome::Ok),
            Some(3) => Ok(Outcome::Unsupported),
            _ => Err(format!(
                "{op} {args:?}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )),
        }
    }
}

/// The scenario every client runs. Returns the pushes it could not run.
fn scenario(env: &Env, client: &Client, base: &str, repo: &str) -> Result<Vec<String>, String> {
    let work = env.root.join(format!("{}-{repo}", client.name));
    std::fs::create_dir_all(&work).unwrap();
    let url = format!("{base}/{repo}.git");
    let reference = format!("{}/{repo}.git", env.http);

    // Seed with the system git over HTTP: branches, a merge, an annotated tag, a
    // binary file larger than one SSH packet.
    let seed = work.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    env.git(&seed, &["init", "-q", "-b", "main"]);
    let noise: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
        .collect();
    std::fs::write(seed.join("blob.bin"), &noise).unwrap();
    for i in 0..4 {
        std::fs::write(seed.join("f"), format!("{i}")).unwrap();
        env.git(&seed, &["add", "."]);
        env.git(&seed, &["commit", "-qm", &format!("c{i}")]);
    }
    env.git(&seed, &["checkout", "-qb", "side", "HEAD~2"]);
    std::fs::write(seed.join("s"), "side").unwrap();
    env.git(&seed, &["add", "s"]);
    env.git(&seed, &["commit", "-qm", "side"]);
    env.git(&seed, &["checkout", "-q", "main"]);
    env.git(&seed, &["merge", "-q", "--no-edit", "side"]);
    env.git(&seed, &["tag", "-am", "release", "v1"]);
    env.git(&seed, &["push", "-q", &reference, "main", "side", "v1"]);

    // Clone.
    let clone = work.join("clone");
    expect_ok(env.op(client, &work, "clone", &[&url, clone.to_str().unwrap()]))?;
    env.git(&clone, &["fsck", "--no-progress"]);
    let head = env.git(&seed, &["rev-parse", "main"]);
    if env.git(&clone, &["rev-parse", "HEAD"]) != head {
        return Err("clone checked out the wrong commit".into());
    }
    if std::fs::read(clone.join("blob.bin")).ok().as_deref() != Some(&noise[..]) {
        return Err("clone has the wrong file contents".into());
    }

    // Push a new branch, force-update it, delete it. Refspecs name a branch rather
    // than HEAD, which not every client resolves.
    let mut unsupported = Vec::new();
    env.git(&clone, &["checkout", "-qb", "work"]);
    std::fs::write(clone.join("n"), "new").unwrap();
    env.git(&clone, &["add", "n"]);
    env.git(&clone, &["commit", "-qm", "from client"]);
    let pushed = env.git(&clone, &["rev-parse", "HEAD"]);
    let c = clone.to_str().unwrap();
    let remote = |r: &str| env.git(&seed, &["ls-remote", &reference, r]);
    match env.op(
        client,
        &work,
        "push",
        &[c, "refs/heads/work:refs/heads/topic"],
    )? {
        Outcome::Unsupported => unsupported.push("push".to_string()),
        Outcome::Ok => {
            if !remote("refs/heads/topic").starts_with(&pushed) {
                return Err("pushed branch did not land".into());
            }
            env.git(&clone, &["commit", "-q", "--amend", "-m", "amended"]);
            let amended = env.git(&clone, &["rev-parse", "HEAD"]);
            expect_ok(env.op(
                client,
                &work,
                "push",
                &[c, "+refs/heads/work:refs/heads/topic"],
            ))?;
            if !remote("refs/heads/topic").starts_with(&amended) {
                return Err("force push did not land".into());
            }
            expect_ok(env.op(client, &work, "push", &[c, ":refs/heads/topic"]))?;
            if !remote("refs/heads/topic").is_empty() {
                return Err("delete did not land".into());
            }
            // Fast-forward main.
            expect_ok(env.op(
                client,
                &work,
                "push",
                &[c, "refs/heads/work:refs/heads/main"],
            ))?;
            env.git(&seed, &["pull", "-q", "--ff-only", &reference, "main"]);
        }
    }

    // Fetch new history pushed by someone else.
    std::fs::write(seed.join("f"), "later").unwrap();
    env.git(&seed, &["commit", "-qam", "later"]);
    env.git(&seed, &["push", "-q", &reference, "HEAD:main"]);
    let later = env.git(&seed, &["rev-parse", "HEAD"]);
    expect_ok(env.op(client, &work, "fetch", &[c]))?;
    if env.git(&clone, &["rev-parse", "origin/main"]) != later {
        return Err("fetch did not update origin/main".into());
    }
    env.git(&clone, &["fsck", "--no-progress"]);
    Ok(unsupported)
}

fn expect_ok(r: Result<Outcome, String>) -> Result<(), String> {
    match r? {
        Outcome::Ok => Ok(()),
        Outcome::Unsupported => Err("required operation unsupported".into()),
    }
}

#[test]
fn clients_interoperate() {
    let env = Env::start();
    let require = std::env::var("PANDO_COMPAT_REQUIRE").is_ok_and(|v| v == "1");
    let mut clients: Vec<Client> = ["0", "1", "2"]
        .into_iter()
        .map(|p| Client {
            name: format!("git-v{p}"),
            driver: Driver::Git {
                bin: "git".into(),
                protocol: p,
            },
        })
        .collect();
    let mut missing = Vec::new();
    for (name, var) in [
        ("go-git", "PANDO_COMPAT_GOGIT"),
        ("libgit2", "PANDO_COMPAT_LIBGIT2"),
        ("jgit", "PANDO_COMPAT_JGIT"),
        ("gitoxide", "PANDO_COMPAT_GIX"),
        ("old-git", "PANDO_COMPAT_OLD_GIT"),
    ] {
        match std::env::var_os(var).map(PathBuf::from) {
            Some(path) if path.exists() => clients.push(Client {
                name: name.into(),
                driver: if name == "old-git" {
                    Driver::Git {
                        bin: path,
                        protocol: "2",
                    }
                } else {
                    Driver::Helper(path)
                },
            }),
            _ => missing.push(name),
        }
    }

    let mut failures = Vec::new();
    println!("\n{:<10} {:<6} result", "client", "via");
    for client in &clients {
        for (via, base) in [("http", env.http.as_str()), ("ssh", env.ssh.as_str())] {
            let repo = format!("{}-{via}", client.name);
            let result = match scenario(&env, client, base, &repo) {
                Ok(u) if u.is_empty() => "ok".to_string(),
                Ok(u) => format!("ok (unsupported by client: {})", u.join(", ")),
                Err(e) => {
                    failures.push(format!("{} over {via}: {e}", client.name));
                    "FAILED".to_string()
                }
            };
            println!("{:<10} {:<6} {result}", client.name, via);
        }
    }
    for m in &missing {
        println!("{m:<10} -      not installed");
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    assert!(
        !require || missing.is_empty(),
        "required clients missing: {missing:?}"
    );
}
