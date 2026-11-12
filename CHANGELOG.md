# Changelog

## Unreleased

Data directories written by 0.2.0 upgrade in place on first start. Going back to
0.2.0 is not safe once garbage collection has run on the new version.

### Added

- Git LFS over HTTPS and SSH: the batch API, signed per-object transfer URLs,
  `git-lfs-authenticate`, and LFS objects in backup, restore and `verify --full`.
- SHA-256 repositories next to SHA-1 ones: `pando serve --object-format`, and
  `POST /api/repos/<repo>?object-format=sha256` to create an empty repository in
  either format. The admin list reports each repository's format.
- The auth file reloads while serving, within a few seconds of an edit or at once on
  SIGHUP. A bad edit keeps the previous entries.
- A cache for shallow and partial clone answers (`--fetch-cache-max-size`), so
  identical clones generate once.
- `--max-push-size` replaces the fixed 2 GiB push limit.
- Metrics: `pando_push_commits_total`, `pando_fetch_cache_hits_total`,
  `pando_fetch_cache_misses_total`, `pando_lfs_uploaded_bytes_total`,
  `pando_lfs_downloaded_bytes_total`. The admin gc response reports `trimmed`.

### Changed

- Pushes are durable. Packs, their indexes and metadata are fsynced before a push is
  acknowledged, and SQLite runs with `synchronous = FULL`.
- Compaction keeps trunk history on disk about once instead of once per level, has
  no top level, and builds covers from memory-mapped parts. A fresh clone of a trunk
  with N pushes needs O(log N) packs, and a client D pushes behind receives fewer
  than 2D pushes' worth.
- Pushes publish their pack before taking the metadata lock, and pushes waiting on
  the lock share one SQLite commit. Fetches, ref advertisements and generated clones
  read through their own connections and never wait on a push.
- A fetch with packfile URIs gets at most eight, the largest packs, and the rest
  inline in the same response.
- A push advertisement for a repository that does not exist no longer creates it.
  The push itself does.

### Fixed

- History dropped by a trunk rewrite is removed from the compaction cover that still
  held part of it, so it is gone from disk two grace periods after the next gc.
- `--shallow-exclude` naming a branch outside the wanted history no longer depends on
  how packs are laid out.
- A crash while creating the SSH host key can no longer leave an empty key file.
- gc removes staging files left by interrupted LFS uploads.

## 0.2.0

- Drop-in mode: every ref advertised, so stock Git workflows, force pushes, leases
  and deletes work unchanged. Multiple repositories, created on first push.
- Protocol v0 and v1 next to v2, shallow clones and real partial clones.
- Git over SSH, and an auth file with tokens, SSH keys, access levels and
  per-repository scopes. Signed, expiring pack URLs.
- Garbage collection of unreachable packs, with a grace period.
- Hooks, webhooks, an admin API, `/healthz` and Prometheus `/metrics`.
- `pando backup`, `pando restore` and `pando verify`. A size limit for Express caches.
- Force-pushing trunk and changing the default branch.
- A client compatibility matrix in CI (git 2.26, libgit2, go-git, JGit, gitoxide),
  release binaries, a Docker image and a systemd unit.
