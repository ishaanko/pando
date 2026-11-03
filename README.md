# pando

A Git backend built from first principles after OpenAI's
[Pando](https://openai.com/index/scaling-git-at-openai). Stock `git` clients push and
fetch as usual. The server holds no repository: pushes become immutable packs plus ref
versions in a metadata store, and fetches are answered by pointing clients at stored
packs.

- **Per-ref consistency.** Each push is a CAS on one ref pointer. Pushes to different
  branches never contend.
- **Reusable packs.** Only trunk is advertised to pushers, so every pushed pack depends
  on trunk alone, and the same bytes can be served to every reader.
- **CDN-style reads.** Fetches return `packfile-uris` to an Express cache tier, chosen
  per pack by rendezvous hashing.
- **LSM compaction.** Trunk packs roll up into L1..L4 covers, so catching up costs
  O(log Δ) packs.

See [docs/design.md](docs/design.md) for the design.

## Run

```sh
cargo run --release -- serve --data ./pando-data --listen 127.0.0.1:8080

git push http://127.0.0.1:8080/repo.git main
git clone http://127.0.0.1:8080/repo.git                     # inline pack
git -c fetch.uriprotocols=http clone http://127.0.0.1:8080/repo.git  # via packfile URIs
```

Fetching requires Git protocol v2 (the default since Git 2.26).

Add Express nodes in front of the blob store:

```sh
pando express --listen 127.0.0.1:8081 --origin http://127.0.0.1:8080 --cache ./cache-a
pando express --listen 127.0.0.1:8082 --origin http://127.0.0.1:8080 --cache ./cache-b
pando serve --express http://127.0.0.1:8081 --express http://127.0.0.1:8082
```

## Working with side branches

Pushers see only trunk, so stock Git treats every side-branch push as a create:

| You want to | Run |
| --- | --- |
| push or fast-forward a branch | `git push origin topic` |
| overwrite a branch (non-fast-forward) | `git push -o force origin topic` |
| delete a branch | `git push origin :refs/heads/topic` |

Trunk (`--trunk`, default `refs/heads/main`) is fast-forward only and cannot be deleted.

## Test

```sh
cargo test                                          # E2E: real git against the server
cargo run --release --example swarm -- 128 5        # concurrent agents
```

Swarm results on a 16-core WSL2 box, including all `git` client process overhead:

```
128 agents x 5 pushes, trunk at v161
side push      n=640   p50=  157.7ms  p90=  235.4ms  p99=  319.4ms
trunk push     n=160   p50=   13.2ms  p90=   14.4ms  p99=  121.4ms
trunk fetch    n=320   p50=   59.4ms  p90=  136.4ms  p99=  294.8ms
800 pushes in 7.4s (108 pushes/s, git client process time included)
```

## Layout

| File | Role |
| --- | --- |
| `src/server.rs` | smart HTTP: v2 `ls-refs`/`fetch`, receive-pack, push validation |
| `src/planner.rs` | haves/wants to ref versions to stored packs |
| `src/meta.rs` | metadata store: ref pointers, versions, commit index, covers |
| `src/pack.rs` | single-pass pack decode, delta resolution, pack splicing |
| `src/compact.rs` | L1..L4 trunk covers |
| `src/express.rs` | pack cache tier, range requests, rendezvous hashing |
| `src/store.rs` | immutable content-addressed pack storage |
