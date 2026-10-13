---
section: Fixed
issues: [#5]
---
- **A failed publish-time lookup no longer blocks old PyPI wheels and npm tarballs with a terminal 451** (#5). The download age gate reads the version's publish time from `/pypi/<project>/json` or the npm packument. When that read failed (timeout, upstream 5xx, connection failure, a shed metadata read, or an unparseable packument), the error was discarded, the version was treated as younger than the threshold, a pending review was filed, and the client got `451 age_gate_blocked`, which installers do not retry. The download now gets a retryable `503 age_gate_unavailable`, still failing closed, files no review, and logs the lookup error.
