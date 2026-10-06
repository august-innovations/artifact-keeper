---
section: Security
issues: [#2]
---
- **`wasmtime` and `wasmtime-wasi` are bumped to 36.0.17** (#2). Closes RUSTSEC-2026-0321, RUSTSEC-2026-0322, and RUSTSEC-2026-0323 in the WASI preview used by the plugin runtime. The lockfile moves the wasmtime, cranelift, pulley, wiggle, and winch families together from 36.0.16 to 36.0.17.
