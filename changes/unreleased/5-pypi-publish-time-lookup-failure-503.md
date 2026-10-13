---
section: Fixed
issues: [#5]
---
- **A failed PyPI publish-time lookup no longer blocks old wheels with a terminal 451.** When the `/pypi/<project>/json` fetch behind the download age gate errored (timeout, upstream 5xx, connection failure), the error was discarded, the version was treated as younger than the threshold, a pending review was filed, and the client got `451 age_gate_blocked`, which installers do not retry. The download now gets a retryable `503 age_gate_unavailable`, still failing closed, files no review, and logs the lookup error.
