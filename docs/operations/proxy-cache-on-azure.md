# The proxy cache on Azure Blob storage

Remote (proxy) repositories cache what they fetch from upstream in the proxy
cache, a key space (`proxy-cache/...`) on one storage backend for the whole
deployment. With `STORAGE_BACKEND` set to `filesystem`, `s3` or `gcs` the
proxy cache lives on that backend. Azure had no proxy-cache support before
1.12.0: since 1.11.0 (#3923) an Azure deployment keeps its proxy cache on the
local filesystem under `STORAGE_PATH`, which with more than one replica must
be shared storage (a ReadWriteMany volume), or purges, quarantine and scans
reach only one replica's copy.

## Turning it on

| Variable | Default | Meaning |
|---|---|---|
| `AZURE_PROXY_CACHE_ENABLED` | `false` | With `STORAGE_BACKEND=azure`, keep the proxy cache in the Azure container instead of under `STORAGE_PATH` (#4320). |

It uses the same account, container, endpoint and credentials as the rest of
the deployment (`AZURE_STORAGE_*`). With it on:

- a remote repository created without `storage_backend` is created on `azure`,
  and one created with `"storage_backend": "azure"` is accepted (both were
  refused or moved to `filesystem` before);
- every replica reads and writes the same cached objects, so `STORAGE_PATH` no
  longer needs to be shared storage for the proxy cache.

It is off by default because turning it on moves the cache: objects cached
under `STORAGE_PATH` before are not migrated. After the switch each remote
repository starts with a cold cache and refetches from upstream on first use;
the files under `STORAGE_PATH/proxy-cache` can then be deleted. Content that is
gone upstream and only existed in the old cache is not carried over, so check
`on_upstream_not_found: keep` repositories before switching.

## What the backend needs from Azure

The proxy cache writes each object to a private staging key and publishes it
with a server-side copy (Put Blob From URL), lists blobs for cache listings and
purges (List Blobs), and deletes evicted objects. With Shared Key auth (an
access key) the copy's destination is authorized with a signed
`Authorization` header and List Blobs with a container SAS; with a token
credential (managed identity or service principal) both use the bearer token,
like every other request. An endpoint that does not implement Put Blob From
URL (it answers 501, as the Azurite emulator does) gets a streamed copy
instead. The identity needs read, write,
delete and list on the container (`Storage Blob Data Contributor`).
