//! A swarm of agents against a Git server: each agent clones, then repeatedly commits
//! and pushes to its own branch while one writer keeps advancing trunk and readers
//! keep fetching it. Prints push/fetch latency percentiles.
//!
//!     cargo run --release --example swarm -- [pando|git] [agents] [pushes-per-agent]
//!
//! `git` runs the same workload against stock `git http-backend` (the smart-HTTP
//! server that ships with Git) on a bare repository, as the baseline.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::serve::ListenerExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@a",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(dir: &Path, n: usize) {
    std::fs::write(
        dir.join(format!("file-{}", n % 7)),
        format!("{n}\n").repeat(1 + n % 50),
    )
    .unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", &format!("change {n}")]);
}

fn timed(samples: &Mutex<Vec<Duration>>, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    samples.lock().unwrap().push(t.elapsed());
}

fn report(name: &str, samples: &Mutex<Vec<Duration>>) {
    let mut s = samples.lock().unwrap().clone();
    s.sort();
    let pct = |p: f64| s[((s.len() - 1) as f64 * p) as usize].as_secs_f64() * 1000.0;
    println!(
        "{name:<14} n={:<5} p50={:>7.1}ms  p90={:>7.1}ms  p99={:>7.1}ms",
        s.len(),
        pct(0.5),
        pct(0.9),
        pct(0.99)
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let backend = args.next().unwrap_or_else(|| "pando".into());
    let mut nums = args.map(|a| a.parse::<usize>().unwrap());
    let agents = nums.next().unwrap_or(32);
    let pushes = nums.next().unwrap_or(5);

    let dir = tempfile::TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = match backend.as_str() {
        "pando" => {
            let (app, router) = pando::server::open(
                &dir.path().join("data"),
                "refs/heads/main",
                &base,
                Vec::new(),
            )
            .unwrap();
            let compactor = app.clone();
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(async move {
                        pando::server::spawn_compactor(compactor, Duration::from_secs(1))
                    })
            });
            router
        }
        "git" => {
            let root = dir.path().join("git-root");
            std::fs::create_dir(&root).unwrap();
            git(&root, &["init", "-q", "--bare", "repo.git"]);
            axum::Router::new()
                .fallback(http_backend)
                .with_state(Arc::new(root))
        }
        other => panic!("unknown backend {other}, expected pando or git"),
    };
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .unwrap()
                    .tap_io(|tcp| {
                        let _ = tcp.set_nodelay(true);
                    });
                axum::serve(listener, router).await.unwrap();
            })
    });
    let url = format!("{base}/repo.git");

    let seed = dir.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    for n in 0..10 {
        commit(&seed, 900_000 + n);
    }
    git(&seed, &["push", "-q", &url, "main"]);

    let side_pushes = Mutex::new(Vec::new());
    let trunk_pushes = Mutex::new(Vec::new());
    let fetches = Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|scope| {
        for i in 0..agents {
            let (url, side_pushes, dir) = (&url, &side_pushes, dir.path());
            scope.spawn(move || {
                let me = dir.join(format!("agent-{i}"));
                git(dir, &["clone", "-q", url, me.to_str().unwrap()]);
                git(&me, &["checkout", "-qb", &format!("agent/{i}")]);
                for n in 0..pushes {
                    commit(&me, 1000 * i + n);
                    timed(side_pushes, || {
                        git(&me, &["push", "-q", "origin", &format!("agent/{i}")])
                    });
                }
            });
        }
        let (url, trunk_pushes, fetches, dir) = (&url, &trunk_pushes, &fetches, dir.path());
        scope.spawn(move || {
            let w = dir.join("trunk-writer");
            git(dir, &["clone", "-q", url, w.to_str().unwrap()]);
            for n in 0..agents * pushes / 4 {
                commit(&w, 500_000 + n);
                timed(trunk_pushes, || git(&w, &["push", "-q", "origin", "main"]));
            }
        });
        for r in 0..4 {
            scope.spawn(move || {
                let me = dir.join(format!("reader-{r}"));
                git(dir, &["clone", "-q", url, me.to_str().unwrap()]);
                for _ in 0..agents * pushes / 8 {
                    timed(fetches, || git(&me, &["fetch", "-q", "origin", "main"]));
                }
            });
        }
    });
    let elapsed = started.elapsed();

    println!("{backend}: {agents} agents x {pushes} pushes");
    report("side push", &side_pushes);
    report("trunk push", &trunk_pushes);
    report("trunk fetch", &fetches);
    let total = side_pushes.lock().unwrap().len() + trunk_pushes.lock().unwrap().len();
    println!(
        "{total} pushes in {:.1}s ({:.0} pushes/s, git client process time included)",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64()
    );
}

/// Bridge HTTP to `git http-backend` over CGI, one process per request, the way a
/// stock Git smart-HTTP deployment runs it.
async fn http_backend(State(root): State<Arc<PathBuf>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &*root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("REMOTE_USER", "swarm") // enables receive-pack
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("CONTENT_TYPE", header("content-type"))
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let out = child.wait_with_output().await.unwrap().stdout;
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("CGI headers");
    let mut resp = Response::builder();
    for line in String::from_utf8_lossy(&out[..split]).lines() {
        let Some((name, value)) = line.split_once(": ") else {
            continue;
        };
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().unwrap().parse().unwrap();
            resp = resp.status(StatusCode::from_u16(code).unwrap());
        } else {
            resp = resp.header(name, value);
        }
    }
    resp.body(Body::from(out[split + 4..].to_vec())).unwrap()
}
