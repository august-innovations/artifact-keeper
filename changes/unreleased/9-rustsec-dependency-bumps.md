---
section: Security
issues: [#9]
---
- **Patched Rust dependencies replace the crates `cargo audit` was failing on** (#9). `wasmtime` and `wasmtime-wasi` move to 36.0.20, and the lockfile picks up the fixed `rustls`, `rustls-webpki`, `webpki-roots`, `libz-sys`, `pulldown-cmark`, `zstd`, and `crypto-bigint` 0.7 lines. sqlx is built without its unused MySQL and SQLite drivers. `crypto-bigint` 0.5.5, `zip` 2/8, and the unused `libsqlite3-sys` lockfile entry have no compatible patched release and stay ignored in `.cargo/audit.toml`.
