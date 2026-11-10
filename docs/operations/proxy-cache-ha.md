# Running remote repositories on more than one replica

This page answers the question in #4331: what is the supported shape for
high availability of remote (proxy) repositories, and what does each storage
choice cost in latency.

## The short answer

- **Object storage is supported for remote repositories.** With
  `STORAGE_BACKEND=s3` or `gcs` the proxy cache lives in the bucket and every
  replica shares it; no shared filesystem is needed. With `STORAGE_BACKEND=azure`
  it does too since 1.12.0 once `AZURE_PROXY_CACHE_ENABLED=true` is set
  (`docs/operations/proxy-cache-on-azure.md`); before that, or without the
  setting, an Azure deployment keeps the proxy cache under `STORAGE_PATH`.
  (The report on #4331 that object storage refuses to start with a remote
  repository describes Azure before 1.11.0, #3923.)
- **A shared filesystem (NFS, a ReadWriteMany volume) is also supported**, and
  is the only option for `STORAGE_BACKEND=filesystem` with more than one
  replica: every replica must see the same `STORAGE_PATH`, or purges,
  quarantine, scans and cache-cap eviction act on one replica's copy.
- **Turn on cross-replica single-flight** on any multi-replica deployment:
  `PROXY_SINGLEFLIGHT_ADVISORY_LOCKS_ENABLED=true`. Without it each replica
  fetches the same cold object from upstream once.

## What a cache hit costs on each backend

A proxied GET that hits the cache reads two objects: the metadata sidecar
(`__cache_meta__.json`) and the body (`__content__`).

- The sidecar is kept in an in-process cache for 30 seconds, so a repeat hit on
  the same replica does not read it again.
- Since 1.12.0 (#4331), on the filesystem backend the body is measured and
  opened at the same time as the sidecar is read, not after it. A first-touch
  hit on NFS costs one storage round trip of latency instead of three (sidecar
  read, size check, open). The buffered path (small metadata documents) reads
  the body alongside the sidecar the same way. The one-instead-of-three figure
  is reasoned from the sequence of storage calls (a test pins that the body is
  opened while the sidecar read is still in flight); it has not been measured
  on NFS. An entry published in the last few seconds is still read
  sequentially, so a replica with a skewed clock never trusts a body opened
  before the publish.
- On S3, GCS and Azure the two reads stay sequential: opening the body there is
  a GET that starts transferring it, which a miss or a stale entry would throw
  away. A presigned redirect (`*_REDIRECT_DOWNLOADS`) moves the body transfer
  off the backend entirely.

What remains is the storage's own per-operation latency. The measurements
reported on #4331 (by the reporter, before this change) put local disk at about
7 ms and NFS at about 40 to 66 ms for the same request; no change in the
backend removes NFS's own round trip. If
latency matters more than a shared cache, two other shapes work:

- object storage with presigned redirects, so replicas share one cache and
  clients fetch bodies from the bucket;
- one replica per site with local disk, each with its own cache, accepting that
  purges and quarantine apply per replica (not recommended where the
  quarantine and purge controls matter).

## Settings that matter with several replicas

| Setting | Why |
|---|---|
| `PROXY_SINGLEFLIGHT_ADVISORY_LOCKS_ENABLED=true` | one upstream fetch per cold object across replicas, not one per replica |
| `PROXY_SINGLEFLIGHT_LOCK_WAIT_TIMEOUT_SECS` | how long a replica waits for another's fill before fetching itself (default 65 s); raise it for very large objects on slow upstreams |
| a shared `STORAGE_PATH` | only for the filesystem backend (and Azure without `AZURE_PROXY_CACHE_ENABLED`) |

The single-flight limits that remain (a slow client paces the fill every other
replica waits for, and a waiting replica cannot serve from a fill in progress)
are described, with a proposed design, in
`docs/plans/2026-10-24-proxy-cross-replica-single-flight-design.md` (#4533).
