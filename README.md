# pando

A Git server built from first principles after OpenAI's
[Pando](https://openai.com/index/scaling-git-at-openai). Any Git client (git, libgit2,
go-git, JGit, gitoxide) clones, fetches and pushes over HTTPS or SSH, unchanged.
Underneath, there is no repository: each push is stored as an immutable pack, and each
branch is a pointer to its latest version. Pushes to different branches never wait on
each other, and fetches are answered with stored packs that every reader shares, like
a CDN.

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

## Operating it

- **Hooks**: executable `pre-receive` and `post-receive` files in `<data>/hooks/` (or
  one repository's `<data>/repos/<repo>.git/hooks/`) run like Git's, so branch
  protection and policy checks work the usual way.
- **Webhooks**: `--webhook <url>` POSTs JSON after every push, signed with
  `--webhook-secret`.
- **Admin API**: `curl -X DELETE -H "Authorization: Bearer $TOKEN" https://git.example.com/api/repos/old`
  (also list, rename, change the default branch, collect garbage).
- **Backups**: `pando backup --data <dir> --out <backup>` while serving,
  `pando restore`, and `pando verify --full`.
- **Monitoring**: `/healthz` and Prometheus `/metrics`.
- **Disk**: unreachable packs (deleted branches, force-pushed history) are deleted
  after an hour (`--gc-grace`). Until then, as on most Git hosts, their commits can
  still be fetched by id. Express cache nodes take `--cache-max-size`.

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
force pushes (including to the default branch), atomic pushes, push options and
mirror imports all work.

## Benchmark

256 agents each push 5 times to their own branch while trunk keeps moving and readers
keep fetching. Same workload against stock `git http-backend` on a bare repository.
16-core machine, `git` client time included.

| | git http-backend | pando |
| --- | --- | --- |
| Side-branch push p50 | 1045 ms | 446 ms |
| Side-branch push p99 | 1827 ms | 948 ms |
| Trunk push p50 | 40 ms | 15 ms |
| Trunk push p99 | 580 ms | 317 ms |
| Trunk fetch p50 | 24 ms | 14 ms |
| Trunk fetch p99 | 660 ms | 557 ms |
| Throughput | 75 pushes/s | 171 pushes/s |

Reproduce with `cargo run --release --example swarm -- git 256 5` and
`cargo run --release --example swarm -- pando 256 5`.

The full git/git history (858k objects, 4,430 refs) imports with `git push --mirror` in
47 s over HTTPS or 60 s over SSH, peaking at 1.4 GB. Cloning it back takes 25 s, and a
`--filter=blob:none` clone takes 15 s.

## Limits

- One machine: metadata is SQLite and packs live on local disk.
- SHA-256 repositories and Git LFS are not supported.
- It is a Git server, not a forge: no web UI or pull requests.
