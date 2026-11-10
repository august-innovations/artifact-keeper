# Artifact upload and download metrics

The backend exports these on `/metrics` (Prometheus text format) alongside the
`ak_http_*` request series. Before 1.11.2 they were defined but never recorded,
so a scrape showed none of them (#4642).

| Metric | Type | Labels | Moves when |
|---|---|---|---|
| `ak_artifact_uploads_total` | counter | `repository`, `format` | an upload's artifact row is committed |
| `ak_artifact_upload_size_bytes` | summary | `format` | the same upload, observing its recorded size in bytes |
| `ak_artifact_downloads_total` | counter | `repository`, `format` | a served download is written to the download statistics |

`repository` is the repository key and `format` its package format (`maven`,
`docker`, `npm`, ...). No label carries an artifact name, path or version, so
the number of series is bounded by the number of repositories.

## What counts as an upload

Every upload path, the generic upload API and each format's native client
(`mvn deploy`, `docker push`, `npm publish`, conda, Go modules and the rest),
records the upload at the same post-commit step that starts scan-on-upload. It
is counted whether or not scanning is configured. One artifact row is one
upload: a Maven deploy of a jar and its POM counts two (the checksum files and
`maven-metadata.xml` are not artifacts), and a `docker push` counts one per
manifest it pushes (blobs are not artifacts). A re-push of an existing
coordinate counts again. Rows the registry writes itself, such as pull-through
cache entries, promotion copies and generated indexes, are not uploads and are
not counted.

For a Docker/OCI manifest the size is the image size (config plus layers), the
same value the artifact row and the UI show.

## What counts as a download

A download is counted when its `download_statistics` row is written, so the
counter agrees with the download counts in the UI and API:

- A `HEAD` request serves no body and is not counted.
- Download events are written by a bounded background queue
  (`DOWNLOAD_EVENT_QUEUE_DEPTH`, `DOWNLOAD_EVENT_FLUSH_WORKERS`). The counter
  moves when a batch is flushed, normally within milliseconds.
- An event shed because the queue was full, or lost because its row could not
  be written, is not counted here; it is counted in
  `ak_download_events_dropped_total{kind="stats"}` instead.

Both counters reset to zero when a backend process restarts; query them with
`increase()` or `rate()` (see
[detection-metrics.md](detection-metrics.md#counters-reset-on-restart)).

## Example queries

```promql
# Uploads per minute, by format
sum by (format) (rate(ak_artifact_uploads_total[5m])) * 60

# The ten most-downloaded repositories over the last day
topk(10, sum by (repository) (increase(ak_artifact_downloads_total[1d])))
```
