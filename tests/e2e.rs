//! End-to-end tests: a real `git` client against an in-process Pando server.

use pando::express::{self, Express, Origin};
use pando::object::Oid;
use pando::planner;
use pando::server::App;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

struct Server {
    url: String,
    app: Arc<App>,
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
            axum::serve(listener, router).await.unwrap();
        });
    });
}

impl Server {
    fn start() -> Server {
        Server::start_with_express(0).0
    }

    /// Start Pando plus `n` Express nodes that fill their caches from Pando.
    /// Returns the Express cache directories.
    fn start_with_express(n: usize) -> (Server, Vec<PathBuf>) {
        let dir = TempDir::new().unwrap();
        let (listener, url) = bind();
        let nodes: Vec<_> = (0..n).map(|_| bind()).collect();
        let urls = nodes.iter().map(|(_, u)| u.clone()).collect();
        let (app, router) = pando::server::open(&dir.path().join("data"), "refs/heads/main", &url, urls).unwrap();
        serve_in_background(listener, router);
        let mut caches = Vec::new();
        for (i, (listener, _)) in nodes.into_iter().enumerate() {
            let cache = dir.path().join(format!("express-{i}"));
            let origin = Origin::Remote { base: url.clone(), client: reqwest::Client::new() };
            let express = Express::new(origin, Some(cache.clone())).unwrap();
            serve_in_background(listener, express::router(Arc::new(express)));
            caches.push(cache);
        }
        (Server { url: format!("{url}/repo.git"), app, dir }, caches)
    }

    /// A fresh working directory for a client.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn compact(&self) -> usize {
        pando::compact::run(&self.app.meta, &self.app.store).unwrap()
    }

    /// Number of objects in the pack behind a ref's current version.
    fn pack_objects(&self, refname: &str) -> usize {
        let v = self.app.meta.lock().unwrap().current(refname).unwrap().unwrap();
        v.pack.map_or(0, |p| self.app.store.read_idx(&p).unwrap().len())
    }

    fn trunk_seq(&self) -> u64 {
        self.app.meta.lock().unwrap().trunk_head().unwrap().map_or(0, |v| v.seq)
    }
}

fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main", "-c", "protocol.version=2"])
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
    assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
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
        .filter_map(|n| n.strip_prefix("pack-")?.strip_suffix(".pack").map(String::from))
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
        assert_eq!(git(repo, &["rev-parse", "HEAD"]), git(&a, &["rev-parse", "HEAD"]));
        git(repo, &["fsck", "--strict"]);
    }
    let trunk_pack = s.app.meta.lock().unwrap().trunk_head().unwrap().unwrap().pack.unwrap();
    assert!(downloaded_pack_names(&by_uri).contains(&trunk_pack.hex()), "URI clone should keep the stored pack");

    // Advance trunk, then fetch incrementally both ways.
    git(&a, &["remote", "add", "origin", &s.url]);
    for i in 0..3 {
        commit(&a, "g", &i.to_string());
        git(&a, &["push", "-q", "origin", "main"]);
    }
    git(&inline, &["pull", "-q", "--ff-only"]);
    git(&by_uri, &["-c", "fetch.uriprotocols=http", "pull", "-q", "--ff-only"]);
    for repo in [&inline, &by_uri] {
        assert_eq!(git(repo, &["rev-parse", "HEAD"]), git(&a, &["rev-parse", "HEAD"]));
        git(repo, &["fsck", "--strict"]);
    }
    // The incremental fetch sent only the three new trunk packs.
    let m = s.app.meta.lock().unwrap();
    let plan = planner::plan(&m, &[m.trunk_head().unwrap().unwrap().tip], &[m.trunk_version(1).unwrap().unwrap().tip]).unwrap();
    assert_eq!((plan.trunk_have, plan.trunk_want, plan.packs.len()), (1, 4, 3));
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
    let v3 = s.app.meta.lock().unwrap().trunk_version(3).unwrap().unwrap().tip;
    git(&stale, &["fetch", "-q", &s.url, &format!("{v3}:refs/heads/old")]);
    git(&stale, &["checkout", "-qb", "topic", "old"]);
    commit(&stale, "topic", "x");
    git(&stale, &["push", "-q", &s.url, "topic"]);
    assert_eq!(s.pack_objects("refs/heads/topic"), 3, "commit + tree + blob, nothing from trunk");

    let topic = s.app.meta.lock().unwrap().current("refs/heads/topic").unwrap().unwrap();
    assert_eq!(topic.trunk_dep, 5, "pack is recorded against the trunk version at push time");

    // A reader that wants only the topic gets its pack plus trunk up to its dependency.
    let reader = s.path("reader");
    init(&reader);
    git(&reader, &["fetch", "-q", &s.url, "topic:topic"]);
    git(&reader, &["fsck", "--strict"]);
    assert_eq!(git(&reader, &["rev-parse", "topic"]), git(&stale, &["rev-parse", "HEAD"]));
}

#[test]
fn per_ref_ordering_and_conflicts() {
    let s = Server::start();
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
    assert!(err.contains("fetch first") || err.contains("rejected"), "{err}");

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
    let v3 = s.app.meta.lock().unwrap().trunk_version(3).unwrap().unwrap().tip;
    git(&behind, &["reset", "-q", "--hard", &v3.hex()]);
    git(&behind, &["update-ref", "refs/remotes/origin/main", &v3.hex()]);

    // 20 versions: five L1 covers and one L2 cover.
    assert_eq!(s.compact(), 6);
    assert_eq!(s.compact(), 0, "compaction is idempotent");
    {
        let m = s.app.meta.lock().unwrap();
        // (0, 20] = L2 [1..16] + L1 [17..20].
        assert_eq!(planner::cover_trunk(&m, 0, 20).unwrap().len(), 2);
        // (3, 20] = v4 + L1 [5..8], [9..12], [13..16], [17..20].
        assert_eq!(planner::cover_trunk(&m, 3, 20).unwrap().len(), 5);
    }

    for uris in [false, true] {
        let fresh = clone(&s, if uris { "fresh-uri" } else { "fresh" }, uris);
        git(&fresh, &["fsck", "--strict"]);
        assert_eq!(git(&fresh, &["rev-parse", "HEAD"]), git(&a, &["rev-parse", "HEAD"]));
    }
    git(&behind, &["-c", "fetch.uriprotocols=http", "pull", "-q", "--ff-only"]);
    git(&behind, &["fsck", "--strict"]);
    assert_eq!(git(&behind, &["rev-parse", "HEAD"]), git(&a, &["rev-parse", "HEAD"]));
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
        .map(|c| std::fs::read_dir(c).map(|d| d.map(|e| e.unwrap().file_name().into_string().unwrap()).collect()).unwrap_or_default())
        .collect();
    let packs: Vec<Oid> = (1..=6).filter_map(|i| s.app.meta.lock().unwrap().trunk_version(i).unwrap().unwrap().pack).collect();
    let nodes: Vec<String> = s.app.express.clone();
    for p in &packs {
        let owner = nodes.iter().position(|n| n == express::pick_node(&nodes, p)).unwrap();
        assert!(cached[owner].contains(&format!("{p}.pack")), "{p} should be cached on node {owner}");
        assert!(!cached[1 - owner].contains(&format!("{p}.pack")));
    }

    // Range requests let an interrupted download resume.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let full = std::fs::read(s.app.store.pack_path(&packs[0])).unwrap();
    let url = format!("{}/pack/{}.pack", express::pick_node(&nodes, &packs[0]), packs[0]);
    let tail = rt.block_on(async {
        let r = reqwest::Client::new().get(&url).header("Range", "bytes=10-").send().await.unwrap();
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
    let row = s.app.meta.lock().unwrap().commit(&Oid::from_hex(&f1).unwrap()).unwrap().unwrap();
    assert_eq!(row.trunk_seq, None, "feature commits are not reachable from trunk");

    // A client that already has trunk (from elsewhere) fetches a side commit by SHA.
    let b = s.path("b");
    init(&b);
    git(&b, &["fetch", "-q", a.to_str().unwrap(), "main:refs/heads/elsewhere"]);
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
    git(s.dir.path(), &["clone", "-q", "--depth", "1", &format!("file://{}", a.display()), shallow.to_str().unwrap()]);
    git(&shallow, &["remote", "set-url", "origin", &s.url]);
    commit(&shallow, "f", "from shallow");
    git(&shallow, &["push", "-q", "origin", "HEAD:main"]);
    let full = clone(&s, "full", false);
    git(&full, &["fsck", "--strict"]);
    assert_eq!(git(&full, &["rev-parse", "HEAD"]), git(&shallow, &["rev-parse", "HEAD"]));
}
