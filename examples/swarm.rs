//! A swarm of agents against an in-process Pando: each agent clones, then repeatedly
//! commits and pushes to its own branch while one writer keeps advancing trunk and
//! readers keep fetching it. Prints push/fetch latency percentiles.
//!
//!     cargo run --release --example swarm -- [agents] [pushes-per-agent]

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=a", "-c", "user.email=a@a", "-c", "init.defaultBranch=main", "-c", "fetch.uriprotocols=http"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn commit(dir: &Path, n: usize) {
    std::fs::write(dir.join(format!("file-{}", n % 7)), format!("{n}\n").repeat(1 + n % 50)).unwrap();
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
    println!("{name:<14} n={:<5} p50={:>7.1}ms  p90={:>7.1}ms  p99={:>7.1}ms", s.len(), pct(0.5), pct(0.9), pct(0.99));
}

fn main() {
    let mut args = std::env::args().skip(1).map(|a| a.parse::<usize>().unwrap());
    let agents = args.next().unwrap_or(32);
    let pushes = args.next().unwrap_or(5);

    let dir = tempfile::TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (app, router) = pando::server::open(&dir.path().join("data"), "refs/heads/main", &base, Vec::new()).unwrap();
    let compactor = app.clone();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            pando::server::spawn_compactor(compactor, Duration::from_secs(1));
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router).await.unwrap();
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
                    timed(side_pushes, || git(&me, &["push", "-q", "origin", &format!("agent/{i}")]));
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

    println!("{agents} agents x {pushes} pushes, trunk at v{}", app.meta.lock().unwrap().trunk_head().unwrap().unwrap().seq);
    report("side push", &side_pushes);
    report("trunk push", &trunk_pushes);
    report("trunk fetch", &fetches);
    let total = side_pushes.lock().unwrap().len() + trunk_pushes.lock().unwrap().len();
    println!("{total} pushes in {:.1}s ({:.0} pushes/s, git client process time included)", elapsed.as_secs_f64(), total as f64 / elapsed.as_secs_f64());
}
