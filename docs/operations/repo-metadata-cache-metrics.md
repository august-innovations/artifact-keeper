# Repository metadata cache metrics

Whole-repository metadata documents (RPM `repomd.xml` and the
`primary`/`filelists`/`other` indexes; hosted conda `repodata.json` with its
encodings, `current_repodata.json`, `run_exports.json`, the stored JLAP,
the CEP-16 shard index and `channeldata.json`) are rendered once per
repository state and served from an in-process cache. Each request checks a
cheap fingerprint of the repository state (RPM: one aggregate query; conda:
the subdir revision, see [conda-hosted-repodata.md](conda-hosted-repodata.md));
when it matches, the cached bytes are served, and when it does not,
one request renders while concurrent requests for the same state wait for
that render instead of starting their own (#2521, #4174).

The backend exports these series on `/metrics` (and
`/api/v1/admin/metrics`) from 1.12.0:

| Metric | Type | Labels | Moves when |
|---|---|---|---|
| `ak_repo_metadata_regenerations_total` | counter | `format`, `scope` | a full render of a metadata set completes |
| `ak_repo_metadata_regeneration_seconds` | summary | `format` | the same render, observing its duration |
| `ak_repo_metadata_cache_requests_total` | counter | `format`, `result` | a metadata request is answered |

- `format` is the package format (`rpm`, `conda`).
- `scope` is the kind of unit a render covers: `root` for RPM (the
  repository, or one metadata root of a repository with a positive
  `repodata_depth`, see [rpm-repodata-depth.md](../rpm-repodata-depth.md));
  for conda `subdir` (every repodata document of one subdir, rendered
  together), `shard_index` (one subdir's CEP-16 shard index) and `channel`
  (`channeldata.json`). It names the kind, not the root or subdir, so the
  number of series stays bounded.
- `result` is `hit` (served from the cache), `coalesced` (waited for another
  request's render of the same state) or `miss` (this request rendered).

No label carries a repository key or path.

## Reading them

One publish, delete or promotion changes the repository state, so the next
metadata request renders once and the counter moves by one, whatever the
number of clients refreshing at that moment. A regeneration rate close to the
request rate means the state changes between almost every read (a bulk import
or a busy CI publisher); each replica renders separately, so the rate also
scales with the replica count.

Renders of a virtual RPM repository are kept per caller-visibility class
(#4346), so callers with different read access each cost one render per
state.

The counters reset when a backend process restarts; query them with
`increase()` or `rate()`.

```promql
# Metadata renders per minute, by format
sum by (format) (rate(ak_repo_metadata_regenerations_total[5m])) * 60

# Share of metadata requests answered without rendering
sum by (format) (rate(ak_repo_metadata_cache_requests_total{result!="miss"}[5m]))
  / sum by (format) (rate(ak_repo_metadata_cache_requests_total[5m]))
```
