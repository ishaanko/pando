# pando

A Git server built from first principles after OpenAI's
[Pando](https://openai.com/index/scaling-git-at-openai). Regular `git` clients push,
fetch, clone (including shallow clones), force-push and delete branches as usual.
Underneath, there is no repository: each push is stored as an immutable pack, and each
branch is a pointer to its latest version. Pushes to different branches never wait on
each other, and fetches are answered with stored packs that every reader shares, like
a CDN.

```sh
cargo run --release -- serve --listen 0.0.0.0:8080 --auth-file tokens

git push http://me:<token>@host:8080/team/app.git main   # repositories are created on first push
git clone http://me:<token>@host:8080/team/app.git
```

`tokens` holds one `<name> <token> <read|write>` per line. Run it behind a TLS proxy.

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

Importing the full git/git history (858k objects, 4,430 refs) with `git push --mirror`
takes 43 s and peaks at 1.4 GB; cloning it back takes 25 s.
