---
section: Changed
issues: [#2]
---
- **Warm npm downloads reuse a version's publish time for five minutes, and buffered metadata reads reserve only the body they hold** (#2). The age gate was reading the packument on every tarball and reserving the 128 MiB ceiling before it knew the document was a few hundred kilobytes, so eight of those reads filled a pod's metadata budget and later arrivals waited long enough to be shed. A cached publish time skips that read. A cache hit or a response that sends Content-Length reserves the real size plus 1 MiB, and a response with no length gives the unused ceiling back once the body arrives.
