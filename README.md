# pando

A Git server built from first principles after OpenAI's
[Pando](https://openai.com/index/scaling-git-at-openai). Any Git client (git, libgit2,
go-git, JGit, gitoxide) clones, fetches and pushes over HTTPS or SSH, unchanged.
Underneath, there is no repository: each push is stored as an immutable pack, and each
branch is a pointer to its latest version. A push to one branch never conflicts with a
push to another, pushes that arrive together share one metadata commit, and fetches
never wait on a push. Fetches are answered with stored packs that every reader shares,
like a CDN.

```sh
cargo install --git https://github.com/ishaanko/pando
pando serve --data /var/lib/pando --listen 0.0.0.0:8080 --ssh-listen 0.0.0.0:2222 \
  --public-url https://git.example.com --auth-file /etc/pando/tokens

git clone https://git.example.com/team/app.git          # repositories are created on first push
git clone ssh://git@git.example.com:2222/team/app.git
```

The auth file says who may do what:

```text
# name   token or SSH public key   access   [repositories]
ishaan   tok_3f9a...               admin
ishaan   ssh-ed25519 AAAAC3...     write
ci       tok_77c1...               read     team/*,docs
```

Tokens go in the password field of HTTPS URLs or git's credential prompt. Put a TLS
proxy in front of the HTTP port (`git.example.com { reverse_proxy 127.0.0.1:8080 }` in
Caddy). Release binaries, a Docker image and a systemd unit are described in
[contrib](contrib/README.md).

## Pack downloads

Git only asks a server for pack URLs when the client opts in:

```sh
git config --global fetch.uriProtocols https
```

Without that setting every fetch still works: the same stored packs arrive spliced
into one response. With it, the eight largest packs of a fetch come as URLs that git
downloads separately, and the small recent ones stay inline. Those URLs point at the
server itself, or at cache nodes you put in front of it:

```sh
pando express --origin https://git.example.com --listen 0.0.0.0:8081 \
  --cache /var/cache/pando --cache-max-size 50G
pando serve ... --express https://packs1.example.com --express https://packs2.example.com
```

Packs are immutable and named by their hash, so a node is a plain read-through cache.
With an auth file the URLs are signed and expire after an hour. Give the nodes the
server's `--url-secret`.

## Operating it

- **Hooks**: executable `pre-receive` and `post-receive` files in `<data>/hooks/` (or
  one repository's `<data>/repos/<repo>.git/hooks/`) run like Git's, so branch
  protection and policy checks work the usual way.
- **Webhooks**: `--webhook <url>` POSTs JSON after every push, signed with
  `--webhook-secret`.
- **Admin API**: `curl -X DELETE -H "Authorization: Bearer $TOKEN" https://git.example.com/api/repos/old`
  (also create, list, rename, change the default branch, collect garbage).
- **Changing access**: edit the auth file while serving. Pando picks the change up
  within a few seconds, or at once on `kill -HUP`. Pushes and fetches in flight are
  unaffected. An edit with a bad line is logged and ignored, and the previous entries
  stay in force. Pack and LFS URLs already handed out stay valid until they expire
  (an hour at most).
- **Durability**: a push is acknowledged only after its pack and metadata are fsynced,
  so an acknowledged push survives power loss.
- **Backups**: `pando backup --data <dir> --out <backup>` while serving,
  `pando restore`, and `pando verify --full`. LFS objects are included.
- **Monitoring**: `/healthz` and Prometheus `/metrics`.
- **Disk**: trunk history is stored about once. Compaction merges trunk pushes into
  larger packs, so a fresh clone needs O(log N) of them, and gc deletes what was
  merged. Unreachable packs (deleted branches, force-pushed history) are deleted after
  an hour (`--gc-grace`). Until then, as on most Git hosts, their commits can still be
  fetched by id. History a force push drops from the default branch is gone from disk
  two grace periods after the next garbage collection (about three hours by default),
  unless a branch or tag still contains it. Express nodes keep cached packs until
  they evict them (`--cache-max-size`).
- **Shallow and partial clones** are generated once and cached per repository
  (`--fetch-cache-max-size`, default 1G, 0 turns it off), so CI jobs cloning the same
  commit with `--depth 1` share one answer.
- **Push size**: `--max-push-size 8G` raises the largest pack one push may send
  (default 2G). A single object is capped at 512 MiB.
- **Git LFS** works over HTTPS and SSH with no client setup beyond `git lfs install`.
  Objects live in `<data>/repos/<repo>.git/lfs/`, travel with the repository through
  rename, delete and backups, and are never garbage collected. File locking is not
  supported; git-lfs turns lock verification off by itself.
- **SHA-256**: `pando serve --object-format sha256` makes repositories created by a
  first push SHA-256 (`git init --object-format=sha256` on the client).
  `curl -X POST -H "Authorization: Bearer $TOKEN" "https://git.example.com/api/repos/team/new?object-format=sha256"`
  creates an empty repository in either format, so one server holds both.

## Compatibility

CI runs the same clone, push, force-push, delete and fetch scenario with every client,
over HTTPS and SSH:

| | HTTPS | SSH |
| --- | --- | --- |
| git, protocol v0, v1 and v2 | ok | ok |
| git 2.26 (oldest with protocol v2) | ok | ok |
| libgit2 (Cargo, IDEs, GUI clients) | ok | ok |
| go-git | ok | ok |
| JGit | ok | ok |
| gitoxide | ok (fetch; it cannot push) | ok (fetch) |

Shallow clones, partial clones (`--filter=blob:none`, `blob:limit`, `tree:<n>`),
force pushes (including to the default branch), atomic pushes, push options, mirror
imports and git-lfs 3.x all work. SHA-256 repositories work with git 2.29 or newer and
with gitoxide (fetch). go-git v5, libgit2 and JGit cannot read SHA-256 repositories
from any server.

## Benchmark

256 agents each push 5 times to their own branch while trunk keeps moving and readers
keep fetching. Same workload against stock `git http-backend` on a bare repository
with git's default settings. 16-core machine, `git` client time included, median of
three runs. Pando fsyncs every push before acknowledging it.

| | git http-backend | pando |
| --- | --- | --- |
| Side-branch push p50 | 1019 ms | 298 ms |
| Side-branch push p99 | 1629 ms | 605 ms |
| Trunk push p50 | 39 ms | 27 ms |
| Trunk push p99 | 428 ms | 197 ms |
| Trunk fetch p50 | 16 ms | 10 ms |
| Trunk fetch p99 | 473 ms | 204 ms |
| Throughput | 78 pushes/s | 115 pushes/s |

Reproduce with `cargo run --release --example swarm -- git 256 5` and
`cargo run --release --example swarm -- pando 256 5`.

The full git/git history (860k objects, 4,447 refs) imports with `git push --mirror` in
47 s over HTTPS or 49 s over SSH, peaking at 1.4 GB. Cloning it back takes 25 s. A
`--filter=blob:none` clone takes 17 s the first time and 6 s once cached.

## Limits

- One machine: metadata is SQLite and packs live on local disk.
- A repository is SHA-1 or SHA-256 from creation. There is no conversion.
- It is a Git server, not a forge: no web UI or pull requests.
