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

128 agents each pushing 5 times to their own branch, while trunk keeps moving and
readers keep fetching. 16-core machine, `git` client time included.

| Operation | Count | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| Side-branch push | 640 | 158 ms | 235 ms | 319 ms |
| Trunk push | 160 | 13 ms | 14 ms | 121 ms |
| Trunk fetch | 320 | 59 ms | 136 ms | 295 ms |

800 pushes in 7.4 s, none rejected. Reproduce with
`cargo run --release --example swarm -- 128 5`.

Design notes are in [docs/design.md](docs/design.md).
