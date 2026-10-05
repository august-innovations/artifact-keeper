---
section: Changed
issues: [#2]
---
- **Warm npm and PyPI downloads skip repeated repository, packument, and virtual-membership reads** (#2). npm and PyPI resolved the repository on every request, and a warm npm tarball also re-read the packument and, for a virtual repository, the member list and scope policy. Those lookups now come from memory for 180 seconds. An admin edit still drops the repository entry and the virtual-layout entry immediately, including a bulk member replace. The first time an npm tarball is stored, the cache records it as gzip; the download response is unchanged. A cache miss still reads the packument so the checksum and any nonstandard tarball URL are known before the bytes are stored.
