---
section: Security
issues: [#4]
---
- **`quinn-proto` is bumped to 0.11.18** (#4). Closes RUSTSEC-2026-0338, RUSTSEC-2026-0339, RUSTSEC-2026-0340, RUSTSEC-2026-0341, and RUSTSEC-2026-0342 in the QUIC stack `reqwest` locks for its optional HTTP/3 feature. The lockfile moves `quinn-proto` from 0.11.15 to 0.11.18.
