# Agent notes

MPP Autopilot is the Rust automation that predicts Ligue 1 scores for the
MPP (Mon Petit Prono) app and runs on a server with Docker. `README.md` gives
the project overview.

## Before finishing a change

- [ ] `cargo fmt`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] No secret in code, logs or commits
- [ ] `docker compose build` still succeeds

## Conventions

- Rust 2024 (MSRV 1.85), idiomatic code, small modules.
- No `unwrap()` on external data (API, env, files); use `?` for fallible
  operations.
- `expect("reason")` is fine when failure would be a programmer error
  (hardcoded values, invariants); `unwrap`/`expect` are fine in tests.
- Avoid `panic!` on reachable runtime paths.
- Typed errors with `thiserror` in modules, `anyhow` in `main`.
- `tracing` for logs; no `println!` outside CLI output.
- Keep network access isolated in `src/mpp.rs` (the MPP app client) and
  `src/elo.rs`.
- Smallest possible visibility (`pub(crate)` by default).
- Document modules (`//!`) and public items (`///`).

## Git workflow

- One feature per branch: `feat/<short-name>`, created from `main`.
- Never work directly on `main` for a feature; initialization and
  documentation are the only exceptions.
- Features land through a pull request.

## Current requirements

- Current scope: Ligue 1. Other competitions may come later.
- MPP Autopilot performs one MPP fetch per day; do not poll.
- `WRITE=false` by default; writing is opt-in.
- State is persisted atomically; never submit twice for the same trigger.
- Docker Compose is the deployment target.
