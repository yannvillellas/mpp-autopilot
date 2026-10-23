# Agent notes

MPP Autopilot is the Rust automation that predicts Ligue 1 scores for the
MPP (Mon Petit Prono) app and runs on a server with Docker. `README.md` gives
the project overview.

## Commands

- Build: `cargo build`
- Run once: `cargo run -- once`
- List fixtures: `cargo run -- list`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`
- Docker: `docker compose build`

## Documentation

- `README.md` is user-facing: keep it short and link to `docs/`.
- `docs/mpp-api.md` records the observed private MPP API contract.
- `docs/architecture.md` explains the runtime flow, model, and state.
- MPP has no public OpenAPI contract; verify API changes against the shipped
  web client or observed responses.

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

## Testing

- Unit tests for private helpers live in a `#[cfg(test)] mod tests` at the
  bottom of the module that owns them.
- Integration tests for public behavior live in `tests/<module>.rs` and use
  only the crate's public API.
- Do not widen visibility only to move a unit test into `tests/`; keep the
  test next to the private code instead.
- Test names describe behavior, not a document or a specification file.

## Comments

- Every module starts with `//!` docs; every public item has `///` docs.
- Private functions get `///` docs only when their contract is not obvious.
- Keep explanatory prose in doc comments. Use `//` inside a function body only
  for a non-obvious invariant, never to restate the code.
- Comments and docs are written in English.

## Security

- Never commit, print, or log `MPP_TOKEN`.
- Keep secrets in `.env` locally and in the deployment environment otherwise.

## Git workflow

- One feature per branch: `feat/<short-name>`, created from `main`.
- Never work directly on `main` for a feature; initialization and
  documentation are the only exceptions.
- Features land through a pull request.

## Current requirements

- Current scope: Ligue 1. Other competitions may come later.
- MPP Autopilot performs one snapshot fetch per day; do not poll outside the
  scheduled triggers. In write mode, each trigger reads the forecast back
  before writing.
- `WRITE=false` by default; writing is opt-in.
- State is persisted atomically. A wrong score is worse than a redundant
  write: never skip a write merely because a previous write is recorded, and
  write anyway when the forecast read fails.
- Docker Compose is the deployment target.

## References

- Rust Book, test organization:
  <https://doc.rust-lang.org/book/ch11-03-test-organization.html>
- Rust API Guidelines, documentation:
  <https://rust-lang.github.io/api-guidelines/documentation.html>
- Serde field attributes, including `alias`:
  <https://serde.rs/field-attrs.html>
