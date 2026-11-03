# Client compatibility matrix

Real non-`git` Git clients clone, fetch and push against Pando. `tests/compat.rs` drives
them through one CLI contract (commits are made by the system `git`; helpers only do transport):

    <helper> clone <url> <dir>
    <helper> fetch <dir>
    <helper> push <dir> <refspec>...    # a:b, +a:b (force), :b (delete)

Exit 0 on success, nonzero on failure, 3 when the client can't do the operation (gix push).
URLs are `http://user:token@127.0.0.1:PORT/repo.git` or `ssh://git@127.0.0.1:PORT/repo.git`
(key in `$PANDO_SSH_KEY`, host keys unchecked).

| helper | client | notes |
| --- | --- | --- |
| `gogit` | go-git v5 | built into the cache with a Go toolchain downloaded there |
| `libgit2/libgit2` | pygit2 (libgit2 + libssh2) | venv in the cache |
| `jgit/jgit` | JGit `jgit.sh` | needs Java 17+ |
| `gix/gix` | gitoxide `gix` | clone/fetch only |

## Run locally

    eval "$(compat/setup.sh)"        # downloads into compat/.cache (idempotent), prints exports
    cargo test --test compat -- --nocapture

Unset `PANDO_COMPAT_*` variables make the matching tests skip; CI sets `PANDO_COMPAT_REQUIRE=1`
so a missing client fails instead. Git 2.26 (`PANDO_COMPAT_OLD_GIT`) is built from source and
needs libcurl dev headers (`curl-config`); `setup.sh` skips it with a message when they are missing.
Use `PANDO_COMPAT_CACHE=/path` (or the first argument of `setup.sh`) to relocate the cache.
