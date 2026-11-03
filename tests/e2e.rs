//! End-to-end tests: a real `git` client against an in-process Pando server.

use axum::serve::ListenerExt;
use pando::express::{self, Express, Origin};
use pando::object::Oid;
use pando::planner;
use pando::server::{Advertise, Config, Repo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

struct Server {
    /// Base URL of the server.
    base: String,
    /// URL of the default test repository.
    url: String,
    server: Arc<pando::server::Server>,
    dir: TempDir,
}

fn bind() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

fn serve_in_background(listener: std::net::TcpListener, router: axum::Router) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(
                listener.tap_io(|tcp| {
                    let _ = tcp.set_nodelay(true);
                }),
                router,
            )
            .await
            .unwrap();
        });
    });
}

impl Server {
    fn start() -> Server {
        Server::start_with(0, |_| {}).0
    }

    fn start_configured(configure: impl FnOnce(&mut Config)) -> Server {
        Server::start_with(0, configure).0
    }

    fn start_with_express(n: usize) -> (Server, Vec<PathBuf>) {
        Server::start_with(n, |_| {})
    }

    /// Start Pando plus `n` Express nodes that fill their caches from Pando.
    /// Returns the Express cache directories.
    fn start_with(n: usize, configure: impl FnOnce(&mut Config)) -> (Server, Vec<PathBuf>) {
        let dir = TempDir::new().unwrap();
        let (listener, url) = bind();
        let nodes: Vec<_> = (0..n).map(|_| bind()).collect();
        let mut cfg = Config::new(dir.path().join("data"), &url);
        cfg.express = nodes.iter().map(|(_, u)| u.clone()).collect();
        cfg.compact_every = None;
        cfg.gc_every = None;
        configure(&mut cfg);
        let server = pando::server::Server::new(cfg).unwrap();
        serve_in_background(listener, server.router());
        let mut caches = Vec::new();
        for (i, (listener, _)) in nodes.into_iter().enumerate() {
            let cache = dir.path().join(format!("express-{i}"));
            let origin = Origin::Remote {
                base: url.clone(),
                client: pando::http_client(),
            };
            let express = Express::new(origin, Some(cache.clone()), None).unwrap();
            serve_in_background(listener, express::router(Arc::new(express)));
            caches.push(cache);
        }
        let repo_url = format!("{url}/repo.git");
        (
            Server {
                base: url,
                url: repo_url,
                server,
                dir,
            },
            caches,
        )
    }

    /// The default test repository (exists after the first push).
    fn repo(&self) -> Arc<Repo> {
        self.server
            .repo("repo")
            .unwrap()
            .expect("repository exists")
    }

    /// A fresh working directory for a client.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn compact(&self) -> usize {
        let r = self.repo();
        pando::compact::run(&r.meta, &r.store).unwrap()
    }

    /// Number of objects in the pack behind a ref's current version.
    fn pack_objects(&self, refname: &str) -> usize {
        let v = self
            .repo()
            .meta
            .lock()
            .unwrap()
            .current(refname)
            .unwrap()
            .unwrap();
        v.pack
            .map_or(0, |p| self.repo().store.read_idx(&p).unwrap().len())
    }

    fn trunk_seq(&self) -> u64 {
        self.repo()
            .meta
            .lock()
            .unwrap()
            .trunk_head()
            .unwrap()
            .map_or(0, |v| v.seq)
    }
}

fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.version=2",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

#[track_caller]
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[track_caller]
fn git_fails(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args);
    assert!(!out.status.success(), "git {args:?} unexpectedly succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Write a file and commit it.
fn commit(dir: &Path, file: &str, contents: &str) -> String {
    std::fs::write(dir.join(file), contents).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", &format!("{file}: {contents}")]);
    git(dir, &["rev-parse", "HEAD"])
}

fn init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
}

fn clone(server: &Server, name: &str, uris: bool) -> PathBuf {
    let dest = server.path(name);
    let mut args = vec![];
    if uris {
        args.extend(["-c", "fetch.uriprotocols=http"]);
    }
    args.extend(["clone", "-q", &server.url, dest.to_str().unwrap()]);
    git(server.dir.path(), &args);
    dest
}

/// Packs the client downloaded through packfile-uris keep their server-side name.
fn downloaded_pack_names(repo: &Path) -> Vec<String> {
    std::fs::read_dir(repo.join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter_map(|n| {
            n.strip_prefix("pack-")?
                .strip_suffix(".pack")
                .map(String::from)
        })
        .collect()
}

#[test]
fn push_then_clone_and_fetch_inline_and_by_uri() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "one");
    commit(&a, "f", "two");
    git(&a, &["push", "-q", &s.url, "main"]);

    let inline = clone(&s, "inline", false);
    let by_uri = clone(&s, "by-uri", true);
    for repo in [&inline, &by_uri] {
        assert_eq!(
            git(repo, &["rev-parse", "HEAD"]),
            git(&a, &["rev-parse", "HEAD"])
        );
        git(repo, &["fsck", "--strict"]);
    }
    let trunk_pack = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_head()
        .unwrap()
        .unwrap()
        .pack
        .unwrap();
    assert!(
        downloaded_pack_names(&by_uri).contains(&trunk_pack.hex()),
        "URI clone should keep the stored pack"
    );

    // Advance trunk, then fetch incrementally both ways.
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..3 {
        commit(&a, "g", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    git(&inline, &["pull", "-q", "--ff-only"]);
    git(
        &by_uri,
        &["-c", "fetch.uriprotocols=http", "pull", "-q", "--ff-only"],
    );
    for repo in [&inline, &by_uri] {
        assert_eq!(
            git(repo, &["rev-parse", "HEAD"]),
            git(&a, &["rev-parse", "HEAD"])
        );
        git(repo, &["fsck", "--strict"]);
    }
    // The incremental fetch sent only the three new trunk packs.
    let r = s.repo();
    let m = r.meta.lock().unwrap();
    let plan = planner::plan(
        &m,
        &[m.trunk_head().unwrap().unwrap().tip],
        &[m.trunk_version(1).unwrap().unwrap().tip],
    )
    .unwrap();
    assert_eq!(
        (plan.trunk_have, plan.trunk_want, plan.packs.len()),
        (1, 4, 3)
    );
}

#[test]
fn side_branch_packs_depend_only_on_trunk() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..5 {
        commit(&a, "base", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }

    // A stale client: it has trunk v3 only, while trunk is at v5. v3 is advertised as a
    // `.have` (tips are sampled at distances 1, 2, 4, ...), so its pack omits all of
    // trunk even though the client never saw v5.
    let stale = s.path("stale");
    init(&stale);
    let v3 = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_version(3)
        .unwrap()
        .unwrap()
        .tip;
    git(
        &stale,
        &["fetch", "-q", &s.url, &format!("{v3}:refs/heads/old")],
    );
    git(&stale, &["checkout", "-qb", "topic", "old"]);
    commit(&stale, "topic", "x");
    git(&stale, &["push", "-q", &s.url, "topic"]);
    assert_eq!(
        s.pack_objects("refs/heads/topic"),
        3,
        "commit + tree + blob, nothing from trunk"
    );

    let topic = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .current("refs/heads/topic")
        .unwrap()
        .unwrap();
    assert_eq!(
        topic.deps,
        pando::meta::Deps {
            trunk: 3,
            packs: vec![]
        },
        "the pack needs exactly trunk up to the client's base, and no other pack"
    );

    // A reader that wants only the topic gets its pack plus trunk up to its dependency.
    let reader = s.path("reader");
    init(&reader);
    git(&reader, &["fetch", "-q", &s.url, "topic:topic"]);
    git(&reader, &["fsck", "--strict"]);
    assert_eq!(
        git(&reader, &["rev-parse", "topic"]),
        git(&stale, &["rev-parse", "HEAD"])
    );
}

#[test]
fn trunk_only_mode_ordering_and_conflicts() {
    // Trunk-only advertisement: side branches are invisible to pushers.
    let s = Server::start_configured(|c| c.advertise = Advertise::Trunk);
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "base");
    git(&a, &["push", "-q", "origin", "main"]);
    let b = clone(&s, "b", false);

    // Trunk: two writers from the same base. The second is told to fetch first.
    commit(&a, "a", "1");
    commit(&b, "b", "1");
    git(&a, &["push", "-q", "origin", "main"]);
    let err = git_fails(&b, &["push", "origin", "main"]);
    assert!(
        err.contains("fetch first") || err.contains("rejected"),
        "{err}"
    );

    // Side branch: same ref, divergent commits. First wins; second needs -o force.
    git(&a, &["checkout", "-qb", "topic"]);
    git(&b, &["checkout", "-qb", "topic", "origin/main"]);
    let a_topic = commit(&a, "t", "from a");
    let b_topic = commit(&b, "t", "from b");
    git(&a, &["push", "-q", "origin", "topic"]);
    let err = git_fails(&b, &["push", "origin", "topic"]);
    assert!(err.contains("non-fast-forward"), "{err}");
    let ls = git(&a, &["ls-remote", "origin", "refs/heads/topic"]);
    assert!(ls.starts_with(&a_topic));
    git(&b, &["push", "-q", "-o", "force", "origin", "topic"]);
    assert!(git(&a, &["ls-remote", "origin", "refs/heads/topic"]).starts_with(&b_topic));

    // A fast-forward update to an existing side branch needs no flag.
    git(&b, &["checkout", "-q", "topic"]);
    let next = commit(&b, "t2", "more");
    git(&b, &["push", "-q", "origin", "topic"]);
    assert!(git(&a, &["ls-remote", "origin", "refs/heads/topic"]).starts_with(&next));

    // Deleting a side branch works with the full ref name; trunk cannot be deleted.
    git(&b, &["push", "-q", "origin", ":refs/heads/topic"]);
    assert_eq!(git(&a, &["ls-remote", "origin", "refs/heads/topic"]), "");
    git_fails(&b, &["push", "origin", ":refs/heads/main"]);
}

#[test]
fn concurrent_pushes_to_different_branches_all_land() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "base");
    git(&a, &["push", "-q", &s.url, "main"]);
    let clients: Vec<PathBuf> = (0..8).map(|i| clone(&s, &format!("c{i}"), false)).collect();
    for (i, c) in clients.iter().enumerate() {
        git(c, &["checkout", "-qb", &format!("agent-{i}")]);
        commit(c, "work", &format!("agent {i}"));
    }
    std::thread::scope(|scope| {
        for (i, c) in clients.iter().enumerate() {
            scope.spawn(move || git(c, &["push", "-q", "origin", &format!("agent-{i}")]));
        }
    });
    let refs = git(&a, &["ls-remote", &s.url, "refs/heads/agent-*"]);
    assert_eq!(refs.lines().count(), 8, "{refs}");
    let all = clone(&s, "all", true);
    git(&all, &["fsck", "--strict"]);
}

#[test]
fn compaction_keeps_catch_up_fetches_small() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..20 {
        commit(&a, &format!("f{}", i % 3), &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    assert_eq!(s.trunk_seq(), 20);
    let behind = clone(&s, "behind", false); // will be moved back to trunk v3 below
    let v3 = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_version(3)
        .unwrap()
        .unwrap()
        .tip;
    git(&behind, &["reset", "-q", "--hard", &v3.hex()]);
    git(
        &behind,
        &["update-ref", "refs/remotes/origin/main", &v3.hex()],
    );

    // 20 versions: five L1 covers and one L2 cover.
    assert_eq!(s.compact(), 6);
    assert_eq!(s.compact(), 0, "compaction is idempotent");
    {
        let r = s.repo();
        let m = r.meta.lock().unwrap();
        // (0, 20] = L2 [1..16] + L1 [17..20].
        assert_eq!(planner::cover_trunk(&m, 0, 20).unwrap().0.len(), 2);
        // (3, 20] = v4 + L1 [5..8], [9..12], [13..16], [17..20].
        assert_eq!(planner::cover_trunk(&m, 3, 20).unwrap().0.len(), 5);
    }

    for uris in [false, true] {
        let fresh = clone(&s, if uris { "fresh-uri" } else { "fresh" }, uris);
        git(&fresh, &["fsck", "--strict"]);
        assert_eq!(
            git(&fresh, &["rev-parse", "HEAD"]),
            git(&a, &["rev-parse", "HEAD"])
        );
    }
    git(
        &behind,
        &["-c", "fetch.uriprotocols=http", "pull", "-q", "--ff-only"],
    );
    git(&behind, &["fsck", "--strict"]);
    assert_eq!(
        git(&behind, &["rev-parse", "HEAD"]),
        git(&a, &["rev-parse", "HEAD"])
    );
}

#[test]
fn annotated_tags_are_peeled() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    let head = commit(&a, "f", "x");
    git(&a, &["tag", "-a", "v1", "-m", "release"]);
    git(&a, &["push", "-q", &s.url, "main", "v1"]);
    let ls = git(&a, &["ls-remote", &s.url]);
    assert!(ls.contains(&format!("{head}\trefs/tags/v1^{{}}")), "{ls}");
    let b = clone(&s, "b", true);
    assert_eq!(git(&b, &["rev-parse", "v1^{commit}"]), head);
    assert!(Oid::from_hex(&git(&b, &["rev-parse", "v1"])).is_some());
}

#[test]
fn express_nodes_cache_packs_and_resume_ranges() {
    let (s, caches) = Server::start_with_express(2);
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..6 {
        commit(&a, "f", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    let b = clone(&s, "b", true);
    git(&b, &["fsck", "--strict"]);

    // Every trunk pack was served by exactly one node, chosen by rendezvous hashing.
    let cached: Vec<Vec<String>> = caches
        .iter()
        .map(|c| {
            std::fs::read_dir(c)
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    let packs: Vec<Oid> = (1..=6)
        .filter_map(|i| {
            s.repo()
                .meta
                .lock()
                .unwrap()
                .trunk_version(i)
                .unwrap()
                .unwrap()
                .pack
        })
        .collect();
    let nodes: Vec<String> = s.server.express_nodes();
    for p in &packs {
        let owner = nodes
            .iter()
            .position(|n| n == express::pick_node(&nodes, p))
            .unwrap();
        assert!(
            cached[owner].contains(&format!("{p}.pack")),
            "{p} should be cached on node {owner}"
        );
        assert!(!cached[1 - owner].contains(&format!("{p}.pack")));
    }

    // Range requests let an interrupted download resume.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let full = std::fs::read(s.repo().store.pack_path(&packs[0])).unwrap();
    let url = format!(
        "{}/pack/{}.pack",
        express::pick_node(&nodes, &packs[0]),
        packs[0]
    );
    let tail = rt.block_on(async {
        let r = pando::http_client()
            .get(&url)
            .header("Range", "bytes=10-")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        r.bytes().await.unwrap()
    });
    assert_eq!(&tail[..], &full[10..]);
}

#[test]
fn side_commits_pushed_with_trunk_stay_off_trunk() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    let base = commit(&a, "f", "base");
    git(&a, &["checkout", "-qb", "feature"]);
    let f1 = commit(&a, "feat", "1");
    commit(&a, "feat", "2");
    git(&a, &["checkout", "-q", "main"]);
    commit(&a, "f", "trunk");
    // One push, one pack, two refs.
    git(&a, &["push", "-q", &s.url, "main", "feature"]);
    let row = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .commit(&Oid::from_hex(&f1).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        row.trunk_seq, None,
        "feature commits are not reachable from trunk"
    );

    // A client that already has trunk (from elsewhere) fetches a side commit by SHA.
    let b = s.path("b");
    init(&b);
    git(
        &b,
        &[
            "fetch",
            "-q",
            a.to_str().unwrap(),
            "main:refs/heads/elsewhere",
        ],
    );
    git(&b, &["fetch", "-q", &s.url, &f1]);
    git(&b, &["cat-file", "-e", &format!("{f1}^{{commit}}")]);
    git(&b, &["fsck", "--strict"]);
    assert_eq!(git(&b, &["rev-parse", &format!("{f1}~1")]), base);
}

#[test]
fn shallow_clones_can_push() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..3 {
        commit(&a, "f", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    let shallow = s.path("shallow");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", a.display()),
            shallow.to_str().unwrap(),
        ],
    );
    git(&shallow, &["remote", "set-url", "origin", &s.url]);
    commit(&shallow, "f", "from shallow");
    git(&shallow, &["push", "-q", "origin", "HEAD:main"]);
    let full = clone(&s, "full", false);
    git(&full, &["fsck", "--strict"]);
    assert_eq!(
        git(&full, &["rev-parse", "HEAD"]),
        git(&shallow, &["rev-parse", "HEAD"])
    );
}

#[test]
fn clone_with_many_branches_sends_gzipped_request() {
    // Git gzips fetch requests over 1 KiB; 40 wants is well past that.
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "base");
    let mut args = vec![
        "push".to_string(),
        "-q".into(),
        s.url.clone(),
        "main".into(),
    ];
    for i in 0..40 {
        git(&a, &["checkout", "-qb", &format!("b{i}"), "main"]);
        commit(&a, "f", &i.to_string());
        args.push(format!("b{i}"));
    }
    git(&a, &args.iter().map(String::as_str).collect::<Vec<_>>());
    let b = clone(&s, "b", false);
    assert_eq!(git(&b, &["branch", "-r"]).lines().count(), 42); // 40 + main + HEAD
    git(&b, &["fsck", "--strict"]);
}

#[test]
fn decoder_matches_git_on_deep_delta_chains() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path();
    init(repo);
    let mut text = String::new();
    for i in 0..120 {
        text.push_str(&format!("line {i} {}\n", "x".repeat(i % 17)));
        std::fs::write(repo.join("big.txt"), &text).unwrap();
        // Objects larger than the decoder's 64 KiB buffer, compressible and not.
        let noise: Vec<u8> = (0..200_000u32)
            .map(|n| (n.wrapping_mul(2654435761) >> (i % 7 + 8)) as u8)
            .collect();
        std::fs::write(repo.join("noise.bin"), &noise).unwrap();
        std::fs::write(repo.join("large.txt"), text.repeat(40)).unwrap();
        std::fs::create_dir_all(repo.join(format!("d{}", i % 5))).unwrap();
        std::fs::write(repo.join(format!("d{}/f", i % 5)), i.to_string()).unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", &i.to_string()]);
    }
    git(repo, &["tag", "-a", "t", "-m", "tag"]);
    git(repo, &["repack", "-adfq", "--depth=50", "--window=50"]);
    let pack_dir = repo.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let parsed = pando::pack::parse(&std::fs::read(&pack).unwrap()).unwrap();

    let expected: std::collections::HashSet<Oid> = git(repo, &["rev-list", "--objects", "--all"])
        .lines()
        .filter_map(|l| Oid::from_hex(&l[..40]))
        .chain(Oid::from_hex(&git(repo, &["rev-parse", "t"])))
        .collect();
    let got: std::collections::HashSet<Oid> = parsed.entries.iter().map(|e| e.oid).collect();
    assert_eq!(got, expected);
    assert!(
        parsed.external_refs().is_empty(),
        "a full pack references nothing outside itself"
    );
    assert_eq!(parsed.commits.len(), 120);
    let name = pack
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("pack-")
        .unwrap()
        .to_string();
    assert_eq!(parsed.hash.hex(), name);
}

// ---------------------------------------------------------------------------------
// Drop-in compatibility: stock git workflows against the default configuration.

fn count(repo: &Path, rev: &str) -> usize {
    git(repo, &["rev-list", "--count", rev]).parse().unwrap()
}

#[test]
fn everyday_branch_workflows_behave_like_git() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "base");
    git(&a, &["push", "-q", "-u", "origin", "main"]);
    let b = clone(&s, "b", false);

    // Two people on one branch: the second is told to fetch first, like any server.
    git(&a, &["checkout", "-qb", "topic"]);
    commit(&a, "t", "a1");
    git(&a, &["push", "-q", "-u", "origin", "topic"]);
    git(&b, &["fetch", "-q"]);
    git(&b, &["checkout", "-q", "-b", "topic", "origin/topic"]);
    commit(&b, "tb", "b1");
    commit(&a, "t", "a2");
    git(&a, &["push", "-q"]);
    let err = git_fails(&b, &["push", "origin", "topic"]);
    assert!(
        err.contains("fetch first") || err.contains("non-fast-forward"),
        "{err}"
    );

    // Rebase and force push.
    git(&b, &["pull", "-q", "--rebase"]);
    git(&b, &["push", "-q", "origin", "topic"]);
    git(&b, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let err = git_fails(&b, &["push", "origin", "topic"]);
    assert!(
        err.contains("non-fast-forward") || err.contains("rejected"),
        "{err}"
    );
    git(&b, &["push", "-q", "-f", "origin", "topic"]);
    assert_eq!(
        git(&a, &["ls-remote", "origin", "refs/heads/topic"])
            .split('\t')
            .next()
            .unwrap(),
        git(&b, &["rev-parse", "HEAD"])
    );

    // --force-with-lease protects against overwriting work you have not seen.
    git(&a, &["fetch", "-q"]);
    git(&a, &["reset", "-q", "--hard", "origin/topic"]);
    commit(&a, "t", "a3");
    git(&a, &["push", "-q"]);
    git(&b, &["commit", "-q", "--amend", "-m", "again"]);
    let err = git_fails(&b, &["push", "--force-with-lease", "origin", "topic"]);
    assert!(err.contains("stale info"), "{err}");
    git(&b, &["fetch", "-q"]);
    git(&b, &["push", "-q", "--force-with-lease", "origin", "topic"]);

    // Delete by short name, then prune elsewhere.
    git(&b, &["push", "-q", "origin", "--delete", "topic"]);
    git(&a, &["fetch", "-q", "--prune"]);
    assert!(!git(&a, &["branch", "-r"]).contains("origin/topic"));

    // Atomic multi-ref push and tags.
    git(&a, &["checkout", "-q", "main"]);
    git(&a, &["pull", "-q", "--ff-only"]);
    git(&a, &["tag", "-a", "v1", "-m", "v1"]);
    git(&a, &["branch", "release"]);
    git(&a, &["push", "-q", "--atomic", "origin", "release", "v1"]);
    let fresh = clone(&s, "fresh", false);
    assert_eq!(git(&fresh, &["describe", "--tags", "origin/release"]), "v1");
    git(&fresh, &["fsck", "--strict"]);
}

#[test]
fn objects_omitted_because_of_other_branches_are_still_served() {
    // With every ref advertised, a pusher leaves out objects it knows the server has
    // on *any* branch. A reader of only trunk must still get them.
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "base");
    git(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["checkout", "-qb", "feature"]);
    commit(&a, "feature.txt", "a blob that only exists on feature");
    commit(&a, "more.txt", "more feature work");
    git(&a, &["push", "-q", "origin", "feature"]);

    // Another client fetches feature, then on main merges it and also reuses the
    // feature-only blob in a new file.
    let b = clone(&s, "b", false);
    git(
        &b,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "merge feature",
            "origin/feature",
        ],
    );
    std::fs::write(b.join("copy.txt"), "a blob that only exists on feature").unwrap();
    git(&b, &["add", "copy.txt"]);
    git(&b, &["commit", "-qm", "reuse blob"]);
    git(&b, &["push", "-q", "origin", "main"]);
    let main = s.repo().meta.lock().unwrap().trunk_head().unwrap().unwrap();
    let merge = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_version(main.seq)
        .unwrap()
        .unwrap();
    assert!(
        !merge.deps.packs.is_empty(),
        "trunk version should depend on the feature pack"
    );

    // Trunk-only readers, before and after compaction, by both transports.
    for round in 0..2 {
        for uris in [false, true] {
            let name = format!("main-only-{round}-{uris}");
            let dest = s.path(&name);
            let mut args = vec![];
            if uris {
                args.extend(["-c", "fetch.uriprotocols=http"]);
            }
            args.extend([
                "clone",
                "-q",
                "--single-branch",
                "--branch",
                "main",
                &s.url,
                dest.to_str().unwrap(),
            ]);
            git(s.dir.path(), &args);
            git(&dest, &["fsck", "--strict"]);
            assert_eq!(
                git(&dest, &["rev-parse", "HEAD"]),
                git(&b, &["rev-parse", "HEAD"])
            );
        }
        for i in 0..4 {
            commit(&b, "pad", &i.to_string());
            git(&b, &["push", "-q", "origin", "main"]);
        }
        s.compact();
    }
}

#[test]
fn shallow_clones_fetch_deepen_and_unshallow() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..10 {
        commit(&a, &format!("f{}", i % 3), &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }

    let c = s.path("shallow");
    git(
        s.dir.path(),
        &["clone", "-q", "--depth", "1", &s.url, c.to_str().unwrap()],
    );
    assert_eq!(count(&c, "HEAD"), 1);
    git(&c, &["fsck"]);

    // New commits arrive in the shallow clone without deepening it.
    for i in 10..13 {
        commit(&a, "g", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    git(&c, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        git(&c, &["rev-parse", "HEAD"]),
        git(&a, &["rev-parse", "HEAD"])
    );
    assert_eq!(count(&c, "HEAD"), 4);

    git(&c, &["fetch", "-q", "--deepen", "3"]);
    assert_eq!(count(&c, "HEAD"), 7);
    git(&c, &["fetch", "-q", "--unshallow"]);
    assert_eq!(count(&c, "HEAD"), 13);
    assert_eq!(git(&c, &["rev-parse", "--is-shallow-repository"]), "false");
    git(&c, &["fsck", "--strict"]);

    // Shallow by time and by exclusion.
    let v5 = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_version(5)
        .unwrap()
        .unwrap()
        .tip;
    git(
        &a,
        &["push", "-q", "origin", &format!("{v5}:refs/heads/old")],
    );
    let ex = s.path("exclude");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            "--shallow-exclude",
            "old",
            "--branch",
            "main",
            &s.url,
            ex.to_str().unwrap(),
        ],
    );
    assert_eq!(count(&ex, "HEAD"), 8);
    let since = s.path("since");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            "--shallow-since",
            "2000-01-01",
            &s.url,
            since.to_str().unwrap(),
        ],
    );
    assert_eq!(count(&since, "HEAD"), 13);

    // A shallow clone can push.
    commit(&c, "h", "from shallow");
    git(&c, &["push", "-q", "origin", "main"]);
    let full = clone(&s, "full", false);
    git(&full, &["fsck", "--strict"]);
}

/// Objects reachable from any ref that a partial clone does not have locally.
fn missing_objects(repo: &Path) -> usize {
    git(repo, &["rev-list", "--objects", "--all", "--missing=print"])
        .lines()
        .filter(|l| l.starts_with('?'))
        .count()
}

#[test]
fn partial_clones_fetch_missing_objects_on_demand() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    std::fs::create_dir_all(a.join("dir/sub")).unwrap();
    for i in 0..4 {
        std::fs::write(a.join("dir/g"), format!("g{i}")).unwrap();
        std::fs::write(a.join("dir/sub/h"), format!("h{i}")).unwrap();
        std::fs::write(a.join("big"), format!("{i}").repeat(100_000)).unwrap();
        git(&a, &["add", "."]);
        commit(&a, "f", &format!("v{i}"));
        git(&a, &["push", "-q", &s.url, "main"]);
    }
    for proto in ["2", "0"] {
        let v = ["-c", &format!("protocol.version={proto}")[..]].map(String::from);
        let run = |dir: &Path, args: &[&str]| {
            let mut all: Vec<&str> = v.iter().map(String::as_str).collect();
            all.extend(args);
            git(dir, &all)
        };
        let dest = |name: &str| s.path(&format!("{name}-v{proto}"));
        let clone_filtered = |name: &str, filter: &str| {
            let d = dest(name);
            run(
                s.dir.path(),
                &[
                    "clone",
                    "-q",
                    &format!("--filter={filter}"),
                    &s.url,
                    d.to_str().unwrap(),
                ],
            );
            d
        };

        // blob:none: history arrives without old blobs, which come on demand.
        let d = clone_filtered("none", "blob:none");
        assert!(missing_objects(&d) > 0, "v{proto}: nothing was filtered");
        run(&d, &["fsck", "--no-progress"]);
        run(&d, &["log", "-p", "-q"]);
        let root = git(&d, &["rev-list", "--max-parents=0", "HEAD"]);
        run(&d, &["checkout", "-q", &root]);
        assert_eq!(std::fs::read_to_string(d.join("dir/g")).unwrap(), "g0");
        run(&d, &["checkout", "-q", "main"]);
        // Fetching more into a partial clone, then filling in everything.
        commit(&a, "f", &format!("more-v{proto}"));
        git(&a, &["push", "-q", &s.url, "main"]);
        run(&d, &["pull", "-q", "--ff-only"]);
        run(&d, &["log", "-p", "--all", "-q"]);
        for c in git(&d, &["rev-list", "--all"]).lines() {
            run(&d, &["archive", "-o", "/dev/null", c]);
        }
        assert_eq!(missing_objects(&d), 0);
        run(&d, &["fsck", "--no-progress"]);

        // blob:limit: only the big blobs are left out.
        let d = clone_filtered("limit", "blob:limit=1k");
        let missing = git(&d, &["rev-list", "--objects", "--all", "--missing=print"]);
        let missing: Vec<&str> = missing.lines().filter(|l| l.starts_with('?')).collect();
        // The current big blob arrived with the checkout.
        assert_eq!(missing.len(), 3, "v{proto}: {missing:?}");

        // tree:0: commits only; trees and blobs arrive on checkout.
        let d = clone_filtered("tree0", "tree:0");
        run(&d, &["log", "-q"]);
        let root = git(&d, &["rev-list", "--max-parents=0", "HEAD"]);
        run(&d, &["checkout", "-q", &root]);
        assert_eq!(std::fs::read_to_string(d.join("dir/sub/h")).unwrap(), "h0");
        run(&d, &["fsck", "--no-progress"]);
    }
}

#[test]
fn protocol_v0_and_v1_clients_work() {
    for proto in ["0", "1"] {
        let s = Server::start();
        let v = format!("protocol.version={proto}");
        let run = |dir: &Path, args: &[&str]| {
            let mut all = vec!["-c", v.as_str()];
            all.extend(args);
            git(dir, &all)
        };
        let a = s.path("a");
        init(&a);
        git(&a, &["remote", "add", "origin", &s.url]);
        for i in 0..5 {
            commit(&a, "f", &format!("v{i}"));
        }
        git(&a, &["tag", "-am", "release", "v1"]);
        git(&a, &["checkout", "-qb", "topic"]);
        commit(&a, "t", "topic");
        git(&a, &["checkout", "-q", "main"]);
        run(&a, &["push", "-q", "origin", "main", "topic", "v1"]);

        let c = s.path("c");
        run(s.dir.path(), &["clone", "-q", &s.url, c.to_str().unwrap()]);
        run(&c, &["fsck", "--no-progress"]);
        assert_eq!(git(&c, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(count(&c, "origin/topic"), 6);
        assert_eq!(
            git(&c, &["rev-parse", "v1^{commit}"]),
            git(&a, &["rev-parse", "main"])
        );
        assert!(run(&c, &["ls-remote", "origin"]).contains("refs/tags/v1^{}"));

        // Incremental fetch, with local commits the server has never seen so the
        // negotiation takes several rounds.
        for i in 0..40 {
            commit(&c, "local", &format!("{i}"));
        }
        for i in 0..3 {
            commit(&a, "f", &format!("w{i}"));
        }
        run(&a, &["push", "-q", "origin", "main"]);
        run(&c, &["fetch", "-q"]);
        run(&c, &["fsck", "--no-progress"]);
        assert_eq!(count(&c, "origin/main"), 8);

        // Shallow clones, deepening, and unshallowing.
        let sh = s.path("shallow");
        run(
            s.dir.path(),
            &["clone", "-q", "--depth", "1", &s.url, sh.to_str().unwrap()],
        );
        assert_eq!(count(&sh, "HEAD"), 1);
        run(&sh, &["fetch", "-q", "--deepen", "2"]);
        assert_eq!(count(&sh, "HEAD"), 3);
        run(&sh, &["fetch", "-q", "--unshallow"]);
        assert_eq!(count(&sh, "HEAD"), 8);
        run(&sh, &["fsck", "--no-progress"]);

        // Pushing from the clone (push is v0 in every git version).
        run(&c, &["push", "-q", "origin", "HEAD:refs/heads/from-clone"]);
        run(&a, &["fetch", "-q"]);
        assert_eq!(count(&a, "origin/from-clone"), 45);
    }
}

#[test]
fn mirror_import_round_trips() {
    // Build a repository with merges, several branches and tags, push it with
    // --mirror, and clone it back.
    let s = Server::start();
    let src = s.path("src");
    init(&src);
    for i in 0..5 {
        commit(&src, "base", &i.to_string());
    }
    for b in ["x", "y"] {
        git(&src, &["checkout", "-qb", b, "main"]);
        for i in 0..3 {
            commit(&src, &format!("{b}.txt"), &i.to_string());
        }
    }
    git(&src, &["checkout", "-q", "main"]);
    git(&src, &["merge", "-q", "--no-ff", "-m", "merge x", "x"]);
    git(&src, &["tag", "-a", "v1", "-m", "v1"]);
    git(&src, &["tag", "light", "y"]);
    git(&src, &["push", "-q", "--mirror", &s.url]);

    let back = s.path("back");
    git(
        s.dir.path(),
        &["clone", "-q", "--mirror", &s.url, back.to_str().unwrap()],
    );
    assert_eq!(git(&back, &["show-ref"]), git(&src, &["show-ref"]));
    git(&back, &["fsck", "--strict"]);
}

#[test]
fn repositories_are_isolated_and_trunk_follows_the_first_push() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["checkout", "-qb", "master"]);
    commit(&a, "f", "a");
    git(
        &a,
        &["push", "-q", &format!("{}/org/alpha.git", s.base), "master"],
    );
    let b = s.path("b");
    init(&b);
    commit(&b, "f", "b");
    git(&b, &["push", "-q", &format!("{}/beta", s.base), "main"]);

    let alpha = s.path("alpha");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            &format!("{}/org/alpha.git", s.base),
            alpha.to_str().unwrap(),
        ],
    );
    assert_eq!(git(&alpha, &["symbolic-ref", "HEAD"]), "refs/heads/master");
    assert_eq!(
        git(&alpha, &["rev-parse", "HEAD"]),
        git(&a, &["rev-parse", "HEAD"])
    );
    assert!(!git(&alpha, &["ls-remote", "origin"]).contains("refs/heads/main"));

    let err = git_fails(
        s.dir.path(),
        &[
            "clone",
            &format!("{}/nope.git", s.base),
            s.path("nope").to_str().unwrap(),
        ],
    );
    assert!(err.contains("not found") || err.contains("404"), "{err}");
}

#[test]
fn tokens_gate_reads_and_writes() {
    let tokens = TempDir::new().unwrap();
    let file = tokens.path().join("tokens");
    std::fs::write(
        &file,
        "# name token access\nci sekrit-read read\nme sekrit-write write\n",
    )
    .unwrap();
    let auth = pando::auth::Auth::load(&file).unwrap();
    let s = Server::start_configured(|c| c.auth = Some(auth));
    let with = |token: &str| {
        s.url
            .replacen("http://", &format!("http://user:{token}@"), 1)
    };

    let a = s.path("a");
    init(&a);
    commit(&a, "f", "x");
    let err = git_fails(&a, &["push", &s.url, "main"]);
    assert!(
        err.contains("401") || err.contains("Authentication") || err.contains("could not read"),
        "{err}"
    );
    let err = git_fails(&a, &["push", &with("sekrit-read"), "main"]);
    assert!(err.contains("403"), "{err}");
    let err = git_fails(&a, &["push", &with("wrong"), "main"]);
    assert!(!err.is_empty());
    git(&a, &["push", "-q", &with("sekrit-write"), "main"]);

    git_fails(
        s.dir.path(),
        &["clone", &s.url, s.path("anon").to_str().unwrap()],
    );
    let r = s.path("reader");
    git(
        s.dir.path(),
        &["clone", "-q", &with("sekrit-read"), r.to_str().unwrap()],
    );
    assert_eq!(
        git(&r, &["rev-parse", "HEAD"]),
        git(&a, &["rev-parse", "HEAD"])
    );
}

#[test]
fn public_read_allows_anonymous_fetch_only() {
    let tokens = TempDir::new().unwrap();
    let file = tokens.path().join("tokens");
    std::fs::write(&file, "me sekrit-write write\n").unwrap();
    let auth = pando::auth::Auth::load(&file).unwrap();
    let s = Server::start_configured(|c| {
        c.auth = Some(auth);
        c.public_read = true;
    });
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "x");
    git(
        &a,
        &[
            "push",
            "-q",
            &s.url.replacen("http://", "http://me:sekrit-write@", 1),
            "main",
        ],
    );
    let anon = clone(&s, "anon", true);
    git(&anon, &["fsck", "--strict"]);
    commit(&anon, "g", "y");
    git_fails(&anon, &["push", "origin", "main"]);
}

// ---------------------------------------------------------------------------------
// Regressions from review.

#[test]
fn clashing_ref_names_are_rejected() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "x");
    git(
        &a,
        &["push", "-q", &s.url, "main", "main:refs/heads/feature"],
    );
    let err = git_fails(&a, &["push", &s.url, "main:refs/heads/feature/x"]);
    assert!(err.contains("conflicts with existing ref"), "{err}");
    let err = git_fails(&a, &["push", &s.url, "main:refs/heads/main/x"]);
    assert!(err.contains("conflicts"), "{err}");
    let err = git_fails(
        &a,
        &["push", &s.url, "main:refs/heads/a", "main:refs/heads/a/b"],
    );
    assert!(err.contains("cannot create both"), "{err}");
    // Existing clones keep fetching cleanly.
    let b = clone(&s, "b", false);
    git(&b, &["fetch", "-q"]);
}

#[test]
fn huge_trunk_fast_forward_is_accepted() {
    // More new commits than the ancestry walk will look up in the index.
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    let n = 100_010;
    let mut script = String::new();
    for i in 1..=n {
        script.push_str(&format!(
            "commit refs/heads/main\nmark :{i}\ncommitter t <t@t> {i} +0000\ndata 0\n{}\n",
            if i > 1 {
                format!("from :{}\n", i - 1)
            } else {
                String::new()
            }
        ));
    }
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(&a)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    git(&a, &["push", "-q", &s.url, "main~100009:refs/heads/main"]);
    git(&a, &["push", "-q", &s.url, "main"]);
    assert_eq!(s.trunk_seq(), 2);
}

#[test]
fn branches_absorbed_into_trunk_stop_being_fetched_separately() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "base");
    git(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["checkout", "-qb", "feat"]);
    commit(&a, "big", "feature payload");
    git(&a, &["push", "-q", "origin", "feat"]);
    // Fast-forward trunk onto the already-pushed branch: an empty pack.
    git(&a, &["checkout", "-q", "main"]);
    git(&a, &["merge", "-q", "--ff-only", "feat"]);
    git(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["push", "-q", "origin", "--delete", "feat"]);
    let blob = Oid::from_hex(&git(&a, &["rev-parse", "HEAD:big"])).unwrap();
    assert!(
        s.repo()
            .meta
            .lock()
            .unwrap()
            .object(&blob)
            .unwrap()
            .unwrap()
            .trunk_seq
            .is_some()
    );

    for i in 0..3 {
        commit(&a, "f", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    // A client one trunk version behind needs exactly one pack.
    let r = s.repo();
    let m = r.meta.lock().unwrap();
    let head = m.trunk_head().unwrap().unwrap();
    let prev = m.trunk_version(head.seq - 1).unwrap().unwrap();
    let plan = planner::plan(&m, &[head.tip], &[prev.tip]).unwrap();
    assert_eq!(plan.packs.len(), 1, "{plan:?}");
}

#[test]
fn shallow_clones_handle_tags_of_trees_and_blobs_and_exclude_tags() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    for i in 0..5 {
        commit(&a, "f", &i.to_string());
        if i == 1 {
            git(&a, &["tag", "v1"]);
        }
    }
    git(&a, &["tag", "treetag", "main^{tree}"]);
    git(&a, &["tag", "-a", "blobtag", "-m", "a blob", "main:f"]);
    git(&a, &["push", "-q", &s.url, "main", "--tags"]);
    let c = s.path("c");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-single-branch",
            &s.url,
            c.to_str().unwrap(),
        ],
    );
    git(&c, &["fsck"]);
    git(&c, &["cat-file", "-e", "treetag"]);
    let ex = s.path("ex");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            "--shallow-exclude=v1",
            &s.url,
            ex.to_str().unwrap(),
        ],
    );
    assert_eq!(count(&ex, "HEAD"), 3);
}

#[test]
fn nested_and_aliased_tags_peel_to_the_commit() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    let head = commit(&a, "f", "x");
    git(&a, &["tag", "-a", "inner", "-m", "inner"]);
    git(&a, &["push", "-q", &s.url, "main", "inner"]);
    git(&a, &["tag", "-a", "outer", "-m", "outer", "inner"]);
    git(&a, &["push", "-q", &s.url, "outer"]);
    git(&a, &["push", "-q", &s.url, "inner:refs/tags/alias"]);
    let ls = git(&a, &["ls-remote", &s.url]);
    for tag in ["inner", "outer", "alias"] {
        assert!(
            ls.contains(&format!("{head}\trefs/tags/{tag}^{{}}")),
            "{tag}: {ls}"
        );
    }
}

#[test]
fn pack_urls_are_signed_when_auth_is_on() {
    let tokens = TempDir::new().unwrap();
    let file = tokens.path().join("tokens");
    std::fs::write(&file, "me sekrit write\n").unwrap();
    let auth = pando::auth::Auth::load(&file).unwrap();
    let s = Server::start_configured(|c| c.auth = Some(auth));
    let authed = s.url.replacen("http://", "http://me:sekrit@", 1);
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "x");
    git(&a, &["push", "-q", &authed, "main"]);

    // Packfile URIs work for an authorized client without forwarding its token.
    let dest = s.path("uri");
    git(
        s.dir.path(),
        &[
            "-c",
            "fetch.uriprotocols=http",
            "clone",
            "-q",
            &authed,
            dest.to_str().unwrap(),
        ],
    );
    git(&dest, &["fsck", "--strict"]);

    let pack = s
        .repo()
        .meta
        .lock()
        .unwrap()
        .trunk_head()
        .unwrap()
        .unwrap()
        .pack
        .unwrap();
    let url = format!("{}/repo.git/pack/{pack}.pack", s.base);
    let status = |u: String| {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { reqwest::get(u).await.unwrap().status().as_u16() })
    };
    assert_eq!(status(url.clone()), 401);
    let signed = s.server.signer.sign("repo", &pack);
    assert_eq!(status(format!("{url}?{signed}")), 200);
    assert_eq!(
        status(format!("{url}?{}", signed.replace("sig=", "sig=00"))),
        401
    );
}

#[test]
fn force_pushing_trunk_starts_a_new_epoch() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..6 {
        commit(&a, "f", &format!("v{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    s.compact();
    // A branch that depends on trunk position 3.
    git(&a, &["checkout", "-qb", "feature", "HEAD~3"]);
    commit(&a, "g", "feature");
    git(&a, &["push", "-q", "origin", "feature"]);
    git(&a, &["checkout", "-q", "main"]);
    let b = clone(&s, "b", false);
    let old_tip = git(&a, &["rev-parse", "HEAD"]);

    // Drop the last two commits and replace them.
    git(&a, &["reset", "-q", "--hard", "HEAD~2"]);
    commit(&a, "f", "rewritten");
    git_fails(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["push", "-qf", "origin", "main"]);
    let epoch = s.repo().meta.lock().unwrap().epoch_start;
    assert_eq!(epoch, 7);

    for (name, uris) in [("c1", false), ("c2", true)] {
        let c = clone(&s, name, uris);
        git(&c, &["fsck", "--no-progress"]);
        assert_eq!(count(&c, "HEAD"), 5);
        assert_eq!(count(&c, "origin/feature"), 4);
    }
    // An old clone sees the forced update.
    git(&b, &["fetch", "-q"]);
    git(&b, &["fsck", "--no-progress"]);
    assert_eq!(
        git(&b, &["rev-parse", "origin/main"]),
        git(&a, &["rev-parse", "HEAD"])
    );

    // Fast-forwards continue in the new epoch, and compaction stays inside it.
    for i in 0..5 {
        commit(&a, "f", &format!("after{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    s.compact();
    let c = clone(&s, "c3", true);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "HEAD"), 10);
    git(&b, &["fetch", "-q"]);
    git(&b, &["fsck", "--no-progress"]);

    // Force trunk back to the old tip without sending objects, then fast-forward it
    // from a clone that still has the old history.
    git(
        &b,
        &[
            "push",
            "-qf",
            "origin",
            &format!("{old_tip}:refs/heads/main"),
        ],
    );
    git(&b, &["checkout", "-q", "--detach", &old_tip]);
    commit(&b, "h", "on old tip");
    git(&b, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let c = clone(&s, "c4", false);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "HEAD"), 7);
    git(&a, &["fetch", "-q"]);
    git(&a, &["fsck", "--no-progress"]);
}

#[test]
fn default_branch_can_change() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..3 {
        commit(&a, "f", &format!("v{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    git(&a, &["checkout", "-qb", "next"]);
    for i in 0..3 {
        commit(&a, "n", &format!("n{i}"));
        git(&a, &["push", "-q", "origin", "next"]);
    }
    let r = s.repo();
    r.meta
        .lock()
        .unwrap()
        .set_trunk("refs/heads/next")
        .unwrap()
        .unwrap();
    let c = clone(&s, "c", true);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(git(&c, &["symbolic-ref", "HEAD"]), "refs/heads/next");
    assert_eq!(count(&c, "HEAD"), 6);
    assert_eq!(count(&c, "origin/main"), 3);
    // Both keep working: next fast-forwards as trunk, main as a branch.
    commit(&a, "n", "n3");
    git(&a, &["push", "-q", "origin", "next"]);
    git(&a, &["checkout", "-q", "main"]);
    commit(&a, "f", "v3");
    git(&a, &["push", "-q", "origin", "main"]);
    git(&c, &["fetch", "-q"]);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "origin/next"), 7);
    assert_eq!(count(&c, "origin/main"), 4);
}

/// Serve SSH for `server` on a random port. Returns the base URL.
fn start_ssh(server: &Arc<pando::server::Server>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = server.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            pando::ssh::serve(server, listener).await.unwrap();
        });
    });
    format!("ssh://git@127.0.0.1:{port}")
}

/// A fresh SSH key. Returns its public key line and a GIT_SSH_COMMAND using it.
fn ssh_key(dir: &Path, name: &str) -> (String, String) {
    let path = dir.join(name);
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&path)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    let command = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
        path.display()
    );
    (public.trim().to_string(), command)
}

fn git_ssh(dir: &Path, ssh: &str, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_SSH_COMMAND", ssh)
        .output()
        .unwrap()
}

#[track_caller]
fn git_ssh_ok(dir: &Path, ssh: &str, args: &[&str]) -> String {
    let out = git_ssh(dir, ssh, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn ssh_clones_fetches_and_pushes_with_keys() {
    let keys = TempDir::new().unwrap();
    let (writer, as_writer) = ssh_key(keys.path(), "writer");
    let (reader, as_reader) = ssh_key(keys.path(), "reader");
    let (scoped, as_scoped) = ssh_key(keys.path(), "scoped");
    let (_, as_stranger) = ssh_key(keys.path(), "stranger");
    let auth = format!("w {writer} write\nr {reader} read\nx {scoped} write other/*\n");
    let s = Server::start_configured(|cfg| {
        cfg.auth = Some(pando::auth::Auth::parse(&auth).unwrap());
    });
    let base = start_ssh(&s.server);
    let url = format!("{base}/team/app.git");

    // Push a history with a large incompressible file, so the pack spans many
    // SSH packets and has to be delimited by its own trailer.
    let a = s.path("a");
    init(&a);
    let noise: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::write(a.join("noise.bin"), &noise).unwrap();
    git(&a, &["add", "noise.bin"]);
    for i in 0..5 {
        commit(&a, "f", &format!("v{i}"));
    }
    git(&a, &["tag", "-am", "release", "v1"]);
    git_ssh_ok(&a, &as_writer, &["push", "-q", &url, "main", "v1"]);

    // Clone over v2 and v0, shallow and partial.
    for proto in ["2", "0"] {
        let v = format!("protocol.version={proto}");
        let c = s.path(&format!("c{proto}"));
        git_ssh_ok(
            s.dir.path(),
            &as_reader,
            &["-c", &v, "clone", "-q", &url, c.to_str().unwrap()],
        );
        git(&c, &["fsck", "--no-progress"]);
        assert_eq!(count(&c, "HEAD"), 5);
        assert_eq!(std::fs::read(c.join("noise.bin")).unwrap(), noise);
        let sh = s.path(&format!("sh{proto}"));
        git_ssh_ok(
            s.dir.path(),
            &as_reader,
            &[
                "-c",
                &v,
                "clone",
                "-q",
                "--depth",
                "2",
                &url,
                sh.to_str().unwrap(),
            ],
        );
        assert_eq!(count(&sh, "HEAD"), 2);
        let pc = s.path(&format!("pc{proto}"));
        git_ssh_ok(
            s.dir.path(),
            &as_reader,
            &[
                "-c",
                &v,
                "clone",
                "-q",
                "--filter=blob:none",
                &url,
                pc.to_str().unwrap(),
            ],
        );
        git_ssh_ok(&pc, &as_reader, &["-c", &v, "log", "-p", "-q"]);
        git(&pc, &["fsck", "--no-progress"]);
    }

    // The writer pushes more, force-pushes a branch, and deletes it.
    commit(&a, "f", "more");
    git_ssh_ok(&a, &as_writer, &["push", "-q", &url, "main"]);
    git(&a, &["checkout", "-qb", "topic"]);
    commit(&a, "t", "1");
    git_ssh_ok(&a, &as_writer, &["push", "-q", &url, "topic"]);
    git(&a, &["commit", "-q", "--amend", "-m", "amended"]);
    git_ssh_ok(&a, &as_writer, &["push", "-qf", &url, "topic"]);
    let c = s.path("c2");
    git_ssh_ok(&c, &as_reader, &["fetch", "-q", "origin"]);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "origin/topic"), 7);
    git_ssh_ok(&a, &as_writer, &["push", "-q", &url, "--delete", "topic"]);
    git_ssh_ok(&c, &as_reader, &["fetch", "-q", "--prune", "origin"]);
    assert!(!git(&c, &["branch", "-r"]).contains("topic"));

    // Access control.
    commit(&c, "f", "from reader");
    let out = git_ssh(
        &c,
        &as_reader,
        &["push", "-q", "origin", "HEAD:refs/heads/r"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("r cannot push to team/app"));
    let out = git_ssh(
        &c,
        &as_scoped,
        &["push", "-q", "origin", "HEAD:refs/heads/x"],
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("x cannot push to team/app"));
    git_ssh_ok(
        &c,
        &as_scoped,
        &[
            "push",
            "-q",
            &format!("{base}/other/thing.git"),
            "HEAD:main",
        ],
    );
    let out = git_ssh(&c, &as_stranger, &["ls-remote", &url]);
    assert!(!out.status.success());
    let out = git_ssh(
        &c,
        &as_reader,
        &["ls-remote", &format!("{base}/missing.git")],
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

/// Every object id in any stored pack of the default repository.
fn stored_objects(s: &Server) -> std::collections::HashSet<String> {
    let repo = s.repo();
    let mut out = std::collections::HashSet::new();
    for (hash, _) in repo.store.list().unwrap() {
        for e in repo.store.read_idx(&hash).unwrap() {
            out.insert(e.oid.hex());
        }
    }
    out
}

#[test]
fn gc_deletes_unreachable_packs_and_keeps_everything_else_servable() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..6 {
        commit(&a, "f", &format!("v{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    // A leaked secret in trunk version 7, compacted into a cover, then a version 8.
    commit(&a, "secret", "hunter2");
    let secret = git(&a, &["rev-parse", "HEAD:secret"]);
    git(&a, &["push", "-q", "origin", "main"]);
    commit(&a, "f", "v7");
    git(&a, &["push", "-q", "origin", "main"]);
    s.compact();
    // A branch built on version 3, and a branch that will be deleted.
    git(&a, &["checkout", "-qb", "keep", "main~5"]);
    commit(&a, "k", "keep");
    git(&a, &["push", "-q", "origin", "keep"]);
    git(&a, &["checkout", "-qb", "doomed", "main"]);
    commit(&a, "d", "doomed");
    let doomed = git(&a, &["rev-parse", "HEAD:d"]);
    git(&a, &["push", "-q", "origin", "doomed"]);
    let old = clone(&s, "old", false);
    git(&a, &["checkout", "-q", "main"]);

    // Nothing is garbage yet.
    let repo = s.repo();
    let r = pando::gc::run(&repo, std::time::Duration::ZERO).unwrap();
    assert_eq!((r.marked, r.deleted), (0, 0));

    // Drop the secret from trunk and delete the branch that contains it.
    git(&a, &["reset", "-q", "--hard", "HEAD~2"]);
    commit(&a, "f", "clean");
    git(&a, &["push", "-qf", "origin", "main"]);
    git(&a, &["push", "-q", "origin", "--delete", "doomed"]);
    assert!(stored_objects(&s).contains(&secret));

    // The pushes already marked what they dropped; within the grace period nothing
    // is deleted.
    let r = pando::gc::run(&repo, std::time::Duration::from_secs(3600)).unwrap();
    assert!(r.marked == 0 && r.deleted == 0, "{r:?}");
    assert!(stored_objects(&s).contains(&secret));
    let r = pando::gc::run(&repo, std::time::Duration::ZERO).unwrap();
    assert!(r.deleted > 0, "{r:?}");
    let left = stored_objects(&s);
    assert!(!left.contains(&secret), "the secret survived gc");
    assert!(!left.contains(&doomed));

    // Everything still reachable is still served, to new and old clones alike.
    for (name, uris) in [("c1", false), ("c2", true)] {
        let c = clone(&s, name, uris);
        git(&c, &["fsck", "--no-progress"]);
        assert_eq!(count(&c, "HEAD"), 7);
        assert_eq!(count(&c, "origin/keep"), 4);
    }
    git(&old, &["fetch", "-q", "--prune"]);
    git(&old, &["fsck", "--no-progress"]);
    // Pushing and compacting keep working afterwards, and a second gc is a no-op.
    for i in 0..4 {
        commit(&a, "f", &format!("after{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    s.compact();
    let r = pando::gc::run(&repo, std::time::Duration::ZERO).unwrap();
    assert_eq!(r.deleted, 0, "{r:?}");
    let c = clone(&s, "c3", true);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "HEAD"), 11);
}

/// One HTTP request from a test. Returns the status and body.
fn http(method: &str, url: &str, token: Option<&str>) -> (u16, String) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut req = pando::http_client().request(method.parse().unwrap(), url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    })
}

fn write_hook(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn hooks_guard_pushes_and_webhooks_report_them() {
    // A webhook receiver that records what it gets.
    let received: Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
    let (listener, hook_url) = bind();
    let sink = received.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
        let sink = sink.clone();
        async move {
            let sig = headers
                .get("x-pando-signature-256")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            sink.lock().unwrap().push((sig, body));
            "ok"
        }
    });
    serve_in_background(listener, app);

    let s = Server::start_configured(|cfg| {
        cfg.auth = Some(pando::auth::Auth::parse("boss tb write\ndev td write\n").unwrap());
        cfg.webhooks = vec![format!("{hook_url}/push")];
        cfg.webhook_secret = Some("shh".into());
    });
    let data = s.dir.path().join("data");
    write_hook(
        &data.join("hooks/pre-receive"),
        "#!/bin/sh\n\
         while read old new ref; do\n\
           if [ \"$ref\" = refs/heads/protected ] && [ \"$PANDO_USER\" != boss ]; then\n\
             echo \"protected is read-only for $PANDO_USER\"; exit 1\n\
           fi\n\
         done\n",
    );
    write_hook(
        &data.join("hooks/post-receive"),
        "#!/bin/sh\n\
         { echo \"$PANDO_REPO $PANDO_USER $GIT_PUSH_OPTION_COUNT $GIT_PUSH_OPTION_0\"; cat; } >> post.log\n\
         echo post-receive saw the push\n",
    );
    let url = |who: &str, tok: &str| s.url.replace("http://", &format!("http://{who}:{tok}@"));
    let a = s.path("a");
    init(&a);
    let main = commit(&a, "f", "1");
    git(&a, &["push", "-q", &url("dev", "td"), "main"]);

    let out = git_cmd(&a, &["push", &url("dev", "td"), "main:protected"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("remote: protected is read-only for dev"),
        "{err}"
    );
    assert!(err.contains("pre-receive hook declined"), "{err}");

    let out = git_cmd(
        &a,
        &[
            "push",
            "-o",
            "ci.skip",
            &url("boss", "tb"),
            "main:protected",
        ],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("remote: post-receive saw the push"), "{err}");

    let log = std::fs::read_to_string(data.join("repos/repo.git/post.log")).unwrap();
    assert!(log.contains("repo dev 0"), "{log}");
    assert!(log.contains("repo boss 1 ci.skip"), "{log}");
    assert!(log.contains(&format!("{} {main} refs/heads/protected", "0".repeat(40))));

    // Two pushes moved refs: two signed deliveries.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while received.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "{got:?}");
    for (sig, body) in &got {
        let want = hex::encode(pando::auth::hmac_sha256(b"shh", body.as_bytes()));
        assert_eq!(sig, &format!("sha256={want}"));
    }
    let boss = got
        .iter()
        .find(|(_, b)| b.contains("\"pusher\":\"boss\""))
        .unwrap();
    assert!(boss.1.contains(&format!(
        r#""ref":"refs/heads/protected","before":"{}","after":"{main}""#,
        "0".repeat(40)
    )));
}

#[test]
fn admin_api_manages_repositories() {
    let s = Server::start_configured(|cfg| {
        cfg.auth = Some(
            pando::auth::Auth::parse("root ta admin\nlead tl admin team/*\nw tw write\n").unwrap(),
        );
    });
    assert_eq!(
        http("GET", &format!("{}/healthz", s.base), None),
        (200, "ok\n".into())
    );
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "1");
    git(&a, &["checkout", "-qb", "next"]);
    commit(&a, "f", "2");
    let push = |repo: &str| {
        let u = format!("{}/{repo}.git", s.base).replace("http://", "http://w:tw@");
        git(&a, &["push", "-q", &u, "main", "next"]);
    };
    for r in ["repo", "team/x", "team/y"] {
        push(r);
    }
    let api = |path: &str| format!("{}/api/repos{path}", s.base);

    let (code, body) = http("GET", &api(""), Some("ta"));
    assert_eq!(code, 200);
    assert_eq!(
        body,
        r#"[{"name":"repo","head":"refs/heads/main","refs":2},{"name":"team/x","head":"refs/heads/main","refs":2},{"name":"team/y","head":"refs/heads/main","refs":2}]"#
    );
    let (_, body) = http("GET", &api(""), Some("tl"));
    assert!(
        !body.contains("\"repo\"") && body.contains("team/x"),
        "{body}"
    );
    assert_eq!(http("GET", &api(""), Some("tw")).0, 403);
    assert_eq!(http("DELETE", &api("/repo"), Some("tl")).0, 403);

    // Default branch.
    assert_eq!(
        http("PUT", &api("/repo/head?branch=next"), Some("ta")).0,
        204
    );
    let c = s.path("c");
    let authed = s.url.replace("http://", "http://w:tw@");
    git(s.dir.path(), &["clone", "-q", &authed, c.to_str().unwrap()]);
    assert_eq!(git(&c, &["symbolic-ref", "HEAD"]), "refs/heads/next");
    assert_eq!(count(&c, "HEAD"), 2);
    assert_eq!(
        http("PUT", &api("/repo/head?branch=nope"), Some("ta")).0,
        400
    );

    // Rename and delete.
    assert_eq!(
        http("POST", &api("/team/x/rename?to=team/z"), Some("tl")).0,
        204
    );
    assert_eq!(
        http("POST", &api("/team/y/rename?to=team/z"), Some("tl")).0,
        400
    );
    let z = s.path("z");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            &format!("{}/team/z.git", s.base).replace("http://", "http://w:tw@"),
            z.to_str().unwrap(),
        ],
    );
    git(&z, &["fsck", "--no-progress"]);
    // A scoped admin cannot move a repository out of its scope.
    assert_eq!(
        http("POST", &api("/team/z/rename?to=stolen"), Some("tl")).0,
        403
    );
    assert_eq!(http("DELETE", &api("/team/y"), Some("tl")).0, 204);
    assert_eq!(http("DELETE", &api("/team/y"), Some("tl")).0, 404);
    let (_, body) = http("GET", &api(""), Some("ta"));
    assert!(body.contains("team/z") && !body.contains("team/x") && !body.contains("team/y"));
    // A pushed name that was deleted starts over as a new repository; names cannot
    // nest one repository inside another.
    push("team/y");
    let nested = format!("{}/team/y.git/inner.git", s.base).replace("http://", "http://w:tw@");
    git_fails(&a, &["push", "-q", &nested, "main"]);

    let (code, body) = http("POST", &api("/repo/gc"), Some("ta"));
    assert_eq!(code, 200);
    assert!(body.starts_with(r#"{"live":"#), "{body}");
    let (_, metrics) = http("GET", &format!("{}/metrics", s.base), None);
    let pushes: u64 = metrics
        .lines()
        .find_map(|l| l.strip_prefix("pando_pushes_total "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(pushes >= 4, "{metrics}");
}

#[test]
fn backups_restore_into_a_working_server() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..6 {
        commit(&a, "f", &format!("v{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    s.compact();
    git(&a, &["checkout", "-qb", "topic"]);
    commit(&a, "t", "1");
    git(&a, &["push", "-q", "origin", "topic"]);
    git(
        &a,
        &["push", "-q", &format!("{}/other.git", s.base), "main"],
    );
    std::fs::create_dir_all(s.dir.path().join("data/hooks")).unwrap();
    std::fs::write(s.dir.path().join("data/hooks/post-receive"), "#!/bin/sh\n").unwrap();

    let backup = s.path("backup");
    let b = pando::backup::backup(&s.dir.path().join("data"), &backup).unwrap();
    assert_eq!(b.repos, 2);
    assert!(backup.join("hooks/post-receive").exists());
    // Pushes after the snapshot do not disturb it.
    commit(&a, "t", "2");
    git(&a, &["push", "-q", "origin", "topic"]);

    let restored = s.path("restored");
    pando::backup::restore(&backup, &restored).unwrap();
    assert!(
        pando::backup::restore(&backup, &restored).is_err(),
        "restored over data"
    );
    let (listener, url) = bind();
    let mut cfg = Config::new(&restored, &url);
    cfg.compact_every = None;
    cfg.gc_every = None;
    serve_in_background(listener, pando::server::Server::new(cfg).unwrap().router());
    let c = s.path("c");
    git(
        s.dir.path(),
        &[
            "clone",
            "-q",
            &format!("{url}/repo.git"),
            c.to_str().unwrap(),
        ],
    );
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "origin/topic"), 7);
    assert_eq!(count(&c, "HEAD"), 6);
    git(
        &c,
        &["push", "-q", "origin", "HEAD:refs/heads/after-restore"],
    );

    // verify notices a missing pack.
    assert!(pando::backup::verify(&backup, true).unwrap().is_empty());
    let packs = backup.join("repos/repo.git/blobs/packs");
    let victim = std::fs::read_dir(&packs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "pack"))
        .unwrap();
    std::fs::remove_file(&victim).unwrap();
    let problems = pando::backup::verify(&backup, false).unwrap();
    assert!(!problems.is_empty());
}

#[test]
fn express_cache_evicts_least_recently_served_packs() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    for i in 0..4 {
        commit(&a, "f", &format!("{i}").repeat(5000));
        git(&a, &["push", "-q", &s.url, "main"]);
    }
    let repo = s.repo();
    let packs: Vec<Oid> = (1..=4)
        .map(|i| {
            repo.meta
                .lock()
                .unwrap()
                .trunk_version(i)
                .unwrap()
                .unwrap()
                .pack
                .unwrap()
        })
        .collect();
    let size = |p: &Oid| std::fs::metadata(repo.store.pack_path(p)).unwrap().len();
    // Room for the two largest packs, not three.
    let mut sizes: Vec<u64> = packs.iter().map(size).collect();
    sizes.sort();
    let limit = sizes[3] + sizes[2];
    let cache = s.path("cache");
    let origin = Origin::Remote {
        base: s.base.clone(),
        client: pando::http_client(),
    };
    let express = Express::new(origin, Some(cache.clone()), None)
        .unwrap()
        .with_max_size(limit);
    let (listener, node) = bind();
    serve_in_background(listener, express::router(Arc::new(express)));
    let get = |p: &Oid| {
        let (code, _) = http("GET", &format!("{node}/repo.git/pack/{p}.pack"), None);
        assert_eq!(code, 200);
    };
    let cached = || -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".pack"))
            .collect();
        v.sort();
        v
    };
    let total = || -> u64 {
        std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum()
    };
    for p in &packs {
        get(p);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(total() <= limit, "cache over its limit: {:?}", cached());
    }
    // The most recently served pack stays, and an evicted one is filled again.
    assert!(cached().contains(&format!("{}.pack", packs[3])));
    assert!(!cached().contains(&format!("{}.pack", packs[0])));
    get(&packs[0]);
    assert!(cached().contains(&format!("{}.pack", packs[0])));
}

#[test]
fn shallow_fetch_can_make_history_shorter() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    for i in 0..10 {
        commit(&a, "f", &i.to_string());
    }
    git(&a, &["push", "-q", &s.url, "main"]);
    for proto in ["2", "0"] {
        let c = s.path(&format!("c{proto}"));
        let v = format!("protocol.version={proto}");
        git(
            s.dir.path(),
            &[
                "-c",
                &v,
                "clone",
                "-q",
                "--depth",
                "5",
                &s.url,
                c.to_str().unwrap(),
            ],
        );
        git(&c, &["-c", &v, "fetch", "-q", "--depth", "1"]);
        assert_eq!(count(&c, "origin/main"), 1, "protocol v{proto}");
        git(&c, &["fsck", "--no-progress"]);
    }
}

#[test]
fn combined_filters_and_trees_shared_across_depths() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    // The same tree at depth 1 (`a`) and depth 2 (`b/a`).
    for dir in ["a", "b/a", "b/c/d"] {
        std::fs::create_dir_all(a.join(dir)).unwrap();
        std::fs::write(a.join(dir).join("x"), "same").unwrap();
    }
    git(&a, &["add", "."]);
    commit(&a, "f", "1");
    git(&a, &["push", "-q", &s.url, "main"]);
    let blob = git(&a, &["rev-parse", "HEAD:a/x"]);
    for (name, filters) in [
        ("tree3", vec!["--filter=tree:3"]),
        (
            "combined",
            vec!["--filter=blob:limit=1m", "--filter=tree:3"],
        ),
    ] {
        let d = s.path(name);
        let mut args = vec!["clone", "-q", "--no-checkout"];
        args.extend(filters);
        args.extend([s.url.as_str(), d.to_str().unwrap()]);
        git(s.dir.path(), &args);
        // a/x sits at depth 2, inside the filter, even though its tree also appears
        // deeper as b/a.
        let missing = git(&d, &["rev-list", "--objects", "--all", "--missing=print"]);
        assert!(!missing.contains(&format!("?{blob}")), "{name}: {missing}");
        git(&d, &["checkout", "-q", "main"]);
        git(&d, &["fsck", "--no-progress"]);
    }
}

#[test]
fn deleted_history_is_not_served_by_id() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "1");
    git(&a, &["push", "-q", &s.url, "main"]);
    git(&a, &["checkout", "-qb", "secret"]);
    let secret = commit(&a, "key", "AWS_KEY=xyz");
    git(&a, &["push", "-q", &s.url, "secret"]);
    git(&a, &["push", "-q", &s.url, "--delete", "secret"]);
    let c = clone(&s, "c", false);
    let err = git_fails(&c, &["fetch", "-q", "origin", &secret]);
    assert!(err.contains("not our ref"), "{err}");
    // Force-pushed trunk history too.
    git(&a, &["checkout", "-q", "main"]);
    let dropped = commit(&a, "f", "2");
    git(&a, &["push", "-q", &s.url, "main"]);
    git(&a, &["reset", "-q", "--hard", "HEAD~1"]);
    commit(&a, "f", "3");
    git(&a, &["push", "-qf", &s.url, "main"]);
    let err = git_fails(&c, &["fetch", "-q", "origin", &dropped]);
    assert!(err.contains("not our ref"), "{err}");
    git(&c, &["fetch", "-q", "origin"]);
    git(&c, &["fsck", "--no-progress"]);
}

#[test]
fn oversized_and_stalled_fetch_requests_do_not_block_the_server() {
    use std::io::Write;
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    let tip = commit(&a, "f", "1");
    git(&a, &["push", "-q", &s.url, "main"]);
    let addr = s.base.trim_start_matches("http://").to_string();

    // Hundreds of requests that start a body and stall.
    let stalled: Vec<std::net::TcpStream> = (0..600)
        .map(|_| {
            let mut t = std::net::TcpStream::connect(&addr).unwrap();
            write!(
                t,
                "POST /repo.git/git-upload-pack HTTP/1.1\r\nHost: x\r\n\
                 Content-Type: application/x-git-upload-pack-request\r\n\
                 Git-Protocol: version=2\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n0014\r\n"
            )
            .unwrap();
            t
        })
        .collect();
    let started = std::time::Instant::now();
    let c = clone(&s, "c", false);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(git(&c, &["rev-parse", "HEAD"]), tip);
    drop(stalled);

    // A request with a million haves is refused, not planned.
    let mut body = Vec::new();
    for line in ["command=fetch\n".to_string(), "object-format=sha1\n".into()] {
        body.extend(format!("{:04x}{line}", line.len() + 4).bytes());
    }
    body.extend(b"0001");
    let want = format!("want {tip}\n");
    body.extend(format!("{:04x}{want}", want.len() + 4).bytes());
    let have = format!("have {}\n", "1".repeat(40));
    let have = format!("{:04x}{have}", have.len() + 4);
    for _ in 0..1_000_000 {
        body.extend(have.bytes());
    }
    body.extend(b"0000");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&body).unwrap();
    let gz = gz.finish().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let text = rt.block_on(async {
        pando::http_client()
            .post(format!("{}/git-upload-pack", s.url))
            .header("Git-Protocol", "version=2")
            .header("Content-Encoding", "gzip")
            .body(gz)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    assert!(text.contains("ERR"), "{text}");
}

#[test]
fn gc_between_trunk_rewrites_keeps_the_repository_servable() {
    for switch_branch in [false, true] {
        let s = Server::start();
        let a = s.path("a");
        init(&a);
        git(&a, &["remote", "add", "origin", &s.url]);
        let c1 = commit(&a, "f", "1");
        git(&a, &["push", "-q", "origin", "main"]);
        commit(&a, "f", "2");
        git(&a, &["push", "-q", "origin", "main"]);
        git(&a, &["reset", "-q", "--hard", &c1]);
        commit(&a, "f", "3");
        git(&a, &["push", "-qf", "origin", "main"]);
        // Removes chain version 2, which nothing needs any more.
        pando::gc::run(&s.repo(), std::time::Duration::ZERO).unwrap();
        if switch_branch {
            git(&a, &["checkout", "-qb", "next"]);
            commit(&a, "g", "next");
            git(&a, &["push", "-q", "origin", "next"]);
            s.repo()
                .meta
                .lock()
                .unwrap()
                .set_trunk("refs/heads/next")
                .unwrap()
                .unwrap();
        } else {
            commit(&a, "f", "4");
            git(&a, &["push", "-q", "origin", "main"]);
            git(&a, &["reset", "-q", "--hard", "HEAD~1"]);
            commit(&a, "f", "5");
            git(&a, &["push", "-qf", "origin", "main"]);
        }
        let c = clone(&s, "c", true);
        git(&c, &["fsck", "--no-progress"]);
        assert_eq!(count(&c, "HEAD"), 3);
    }
}

#[test]
fn gc_never_deletes_a_pack_pushed_again_meanwhile() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "1");
    git(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["checkout", "-qb", "b"]);
    commit(&a, "g", "branch");
    git(&a, &["push", "-q", "origin", "b"]);
    git(&a, &["push", "-q", "origin", "--delete", "b"]);

    // Hold the re-push of `b` (an identical pack) inside its pre-receive hook while
    // gc deletes the old copy.
    let sync = s.path("sync");
    std::fs::create_dir_all(&sync).unwrap();
    write_hook(
        &s.dir.path().join("data/hooks/pre-receive"),
        &format!(
            "#!/bin/sh\ncat >/dev/null\ntouch {0}/started\nwhile [ ! -f {0}/go ]; do sleep 0.05; done\n",
            sync.display()
        ),
    );
    let a2 = a.clone();
    let pusher = std::thread::spawn(move || git_cmd(&a2, &["push", "-q", "origin", "b"]));
    while !sync.join("started").exists() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let r = pando::gc::run(&s.repo(), std::time::Duration::ZERO).unwrap();
    assert_eq!(r.deleted, 1, "{r:?}");
    std::fs::write(sync.join("go"), "").unwrap();
    assert!(pusher.join().unwrap().status.success());
    assert!(
        pando::backup::verify(&s.dir.path().join("data"), true)
            .unwrap()
            .is_empty()
    );
    let c = clone(&s, "c", false);
    git(&c, &["fsck", "--no-progress"]);
    assert_eq!(count(&c, "origin/b"), 2);
    // The re-pushed pack is live again, not left marked.
    assert_eq!(
        pando::gc::run(&s.repo(), std::time::Duration::ZERO)
            .unwrap()
            .deleted,
        0
    );
}

#[test]
fn backups_forget_packs_gc_deleted_during_the_copy() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 1..=4 {
        commit(&a, "f", &format!("{i}"));
        git(&a, &["push", "-q", "origin", "main"]);
    }
    let v4 = git(&a, &["rev-parse", "HEAD"]);
    let repo = s.repo();
    let p4 = repo
        .meta
        .lock()
        .unwrap()
        .trunk_version(4)
        .unwrap()
        .unwrap()
        .pack
        .unwrap();
    git(&a, &["reset", "-q", "--hard", "HEAD~2"]);
    commit(&a, "f", "x");
    git(&a, &["push", "-qf", "origin", "main"]);
    // As if gc deleted v4's pack between the backup's snapshot and its copy.
    repo.store.delete(&p4).unwrap();
    let out = s.path("backup");
    let b = pando::backup::backup(&s.dir.path().join("data"), &out).unwrap();
    assert_eq!(b.skipped, 1);
    let data2 = s.path("data2");
    pando::backup::restore(&out, &data2).unwrap();

    let (listener, url) = bind();
    let mut cfg = Config::new(data2, &url);
    cfg.compact_every = None;
    cfg.gc_every = None;
    serve_in_background(listener, pando::server::Server::new(cfg).unwrap().router());
    let url = format!("{url}/repo.git");
    git(&a, &["checkout", "-q", "--detach", &v4]);
    commit(&a, "g", "on old v4");
    git(&a, &["push", "-q", &url, "HEAD:refs/heads/side"]);
    let c = s.path("c");
    git(
        s.dir.path(),
        &["clone", "-q", "--mirror", &url, c.to_str().unwrap()],
    );
    git(&c, &["fsck", "--no-progress"]);
}

#[test]
fn trunk_only_mode_accepts_fast_forwards_after_a_rewrite() {
    let s = Server::start_configured(|cfg| cfg.advertise = Advertise::Trunk);
    let a = s.path("a");
    init(&a);
    git(&a, &["remote", "add", "origin", &s.url]);
    commit(&a, "f", "1");
    let x = commit(&a, "f", "2");
    git(&a, &["push", "-q", "origin", "main"]);
    commit(&a, "f", "3");
    git(&a, &["push", "-q", "origin", "main"]);
    git(&a, &["push", "-q", "origin", &format!("{x}:refs/heads/b")]);
    git(&a, &["reset", "-q", "--hard", &x]);
    commit(&a, "f", "Y");
    git(&a, &["push", "-qf", "origin", "main"]);
    commit(&a, "f", "Z");
    git(&a, &["push", "-q", "origin", "HEAD:refs/heads/b"]);
}

#[test]
fn only_branches_can_become_the_default() {
    let s = Server::start();
    let a = s.path("a");
    init(&a);
    commit(&a, "f", "1");
    git(&a, &["tag", "-am", "t", "v1"]);
    git(&a, &["push", "-q", &s.url, "main", "v1"]);
    let repo = s.repo();
    let mut meta = repo.meta.lock().unwrap();
    assert!(meta.set_trunk("refs/tags/v1").unwrap().is_err());
    assert!(meta.set_trunk("refs/heads/nope").unwrap().is_err());
}
