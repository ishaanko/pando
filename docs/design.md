# Design

This is a from-scratch build of the Git backend OpenAI described in
[Scaling Git at OpenAI](https://openai.com/index/scaling-git-at-openai) and the
[@OpenAIDevs thread](https://x.com/OpenAIDevs/status/2117408792346509663). The post gives
the invariants but no schema or code. This document covers the parts we had to derive
ourselves and the places where stock Git forced a choice.

## Two assumptions dropped

A normal Git server gives every reader a single consistent snapshot of all refs, and
computes a custom pack for every fetch. Both get expensive once there are thousands of
agents and millions of branches.

| Normal Git server | Pando |
| --- | --- |
| Repository-wide snapshot of refs | Per-ref serializability. Two branch tips in one fetch may come from different moments. |
| One pack computed per client | Stored packs reused across all clients. A client may receive objects it already has. |

## Data model

```
blob store (immutable)          metadata store ("Turtle")
──────────────────────          ─────────────────────────────────────────────
packs/<hash>.pack               refs:      name -> version id      (CAS'd pointer)
packs/<hash>.pidx               versions:  id, ref, seq, tip, pack, trunk_dep, peeled
                                commits:   oid -> first version, trunk_seq, parents
                                covers:    (level, start) -> pack
```

A **ref version** is immutable. A version of ref `R` is complete given:

```
its own pack  +  trunk versions 1..=trunk_dep
```

Trunk is a linear, fast-forward-only chain, so trunk version `n` has `trunk_dep = n - 1`.

### Why that invariant holds

The client decides which objects it leaves out of a push. `send-pack` leaves out
everything reachable from advertised refs that the client already has. Pando advertises
only:

- the trunk ref,
- `.have` lines for older trunk tips at distances 1, 2, 4, 8, ... so stale clients still
  find a base they have,
- `no-thin`, so no delta in the pack refers to a base outside it.

Every advertised object is reachable from the current trunk head, and trunk only moves
forward. So whatever the client left out is in trunk `1..=t`, where `t` is the trunk head
when the push is validated. That `t` becomes the version's `trunk_dep`.

This is what makes packs reusable: a side-branch pack works for any reader that has
trunk up to `trunk_dep`. With a full ref advertisement, a pack could silently depend on
any branch the pusher happened to have fetched.

## Push

```
client ──receive-pack──▶ Pando
  1. stream pack to blob-store staging        (no local repository, no unpacking)
  2. one decode pass: inflate, resolve in-pack deltas, hash every object
       reject thin packs; reject commits whose missing parents are not on trunk
  3. rename into packs/<trailer-hash>.pack + .pidx      (immutable, content addressed)
  4. one metadata transaction per ref:
       CAS refs[R] from the version the validator saw
       INSERT version (tip, pack, trunk_dep)
       UPSERT commit index rows
  5. report-status; wake the compactor if trunk moved
```

The only contended key is `refs[R]`. Pushes to different refs never conflict. Two
pushes to the same ref are ordered by the CAS, and the loser gets a normal Git
rejection.

Because side branches are never advertised, stock Git sends an update to an existing
side branch as a *create* (`old = 0`). Pando accepts it when it fast-forwards the
current tip, which matches the client's own rule for non-forced pushes. A non-fast-forward
needs `git push -o force`. OpenAI's Git fork fixes this with "explicit haves", which
advertise a branch tip for lease checks without making it omittable. Stock Git has no
way to express that.

## Fetch

```
client ──ls-refs──▶ Pando              (current versions, filtered by ref-prefix)
client ──fetch want/have──▶ Pando
  haves  -> versions the client has   (exact tips, or a bounded walk to one)
            trunk collapses to one number: trunk_have
  wants  -> versions to send          (trunk collapses to trunk_want)
  trunk (trunk_have, trunk_want]  covered by compaction levels, largest first
  each side version not already had: its pack, plus trunk up to its trunk_dep
Pando ──packfile-uris + empty pack──▶ client ──GET──▶ Express
```

Clients that send `packfile-uris` (stock Git with `fetch.uriprotocols=http,https`) get a
list of `<hash> <url>` lines and download the packs from Express. Clients that don't
get the same stored packs spliced into one inline pack. Splicing never inflates
objects. It copies them byte for byte, drops duplicates, and re-encodes ofs-delta
offsets.

Negotiation is stateless (protocol v2 over HTTP). Pando sends `ready` once any have
maps to a known version. Otherwise it NAKs, and the client keeps walking back through
its history.

## Compaction

Without compaction, a client catching up on a day of trunk would download one pack per
push. Covers work like LSM levels:

```
L2  [ 1 ........................ 16 ]
L1  [ 1 .. 4 ][ 5 .. 8 ][ 9 .. 12 ][ 13 .. 16 ][ 17 .. 20 ]
L0    1 2 3 4   5 6 7 8   ...                     17 18 19 20
```

A level-L cover spans `4^L` aligned trunk versions, up to L4 (256 versions). The planner
walks from `trunk_have` and takes the largest aligned cover that fits at each step, a
skip-list walk that needs O(log Δ) packs. Lower levels are kept, so any range can still
be served.

OpenAI's compactor also repacks for better compression. Ours only splices, so a cover
compresses no better than its parts. Re-deltifying across parts would be the next step.

## Express

Express is a read-through cache. A pack is immutable and named by its hash, so caching
needs no invalidation:

- serve from local disk; on a miss, fill from the origin and check the trailer hash
  before publishing into the cache;
- concurrent misses on one pack share a single origin read;
- `Range: bytes=N-` lets an interrupted download resume;
- Pando picks a node per pack by rendezvous hashing, so each node caches a stable slice
  and adding a node only moves that node's share.

## What is simplified

- **Metadata**: SQLite in WAL mode behind a mutex. The access pattern (single-key CAS,
  small transactions, point lookups) maps onto any horizontally scalable KV store. SQLite
  itself serializes writers.
- **Validation**: the pack is fully decoded under size limits (512 MiB per object,
  4 GiB inflated per pack, 2 GiB on the wire). Commit parents and tag targets missing
  from the pack must already be on trunk. Commits only get a trunk position when the
  new trunk tip reaches them, so a side branch pushed alongside trunk stays off trunk.
  Trees and blobs that the pack references but does not contain are not checked one
  by one, which would need an object-level index.
- **Shallow clones** can push, since the same parents-on-trunk check covers them.
  Pando does not serve shallow fetches.
- **Memory**: a pushed pack is decoded in memory, and a multi-pack inline fetch loads
  its packs before splicing. Packfile-URI fetches stream from disk.
- **Not implemented**: serving shallow or partial clones, `want-ref`, SHA-256, the migration
  proxy mode, and Express cache eviction.
