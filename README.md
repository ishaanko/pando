# pando

A Git server built from first principles after OpenAI's
[Pando](https://openai.com/index/scaling-git-at-openai). Regular `git` clients push and
fetch as usual. Underneath, there is no repository: each push is stored as an immutable
pack, and each branch is a pointer to its latest version. Pushes to different branches
never wait on each other, and fetches are answered with stored packs that every reader
shares, like a CDN.

```sh
cargo run --release -- serve --listen 127.0.0.1:8080
git push http://127.0.0.1:8080/repo.git main
git clone http://127.0.0.1:8080/repo.git
```

## Benchmark

256 agents each push 5 times to their own branch while trunk keeps moving and readers
keep fetching. Same workload against stock `git http-backend` on a bare repository.
16-core machine, `git` client time included.

| | git http-backend | pando |
| --- | --- | --- |
| Side-branch push p50 | 1056 ms | 336 ms |
| Side-branch push p99 | 2073 ms | 628 ms |
| Trunk push p50 | 39 ms | 13 ms |
| Trunk push p99 | 609 ms | 129 ms |
| Trunk fetch p50 | 18 ms | 13 ms |
| Trunk fetch p99 | 699 ms | 428 ms |
| Throughput | 76 pushes/s | 201 pushes/s |

Reproduce with `cargo run --release --example swarm -- git 256 5` and
`cargo run --release --example swarm -- pando 256 5`.
