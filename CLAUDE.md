# mlq — developer notes

Containerised GPU/CPU job scheduler for a shared ML server. Rust workspace + React dashboard.

## Layout
- `crates/core` — domain types and the pure scheduler (`scheduler::schedule`). No IO. Unit tests live here.
- `crates/daemon` (`mlqd`) — axum REST/SSE API (`src/api/`), SQLite via sqlx (`store.rs`, `migrations/`),
  scheduler loop + job lifecycle (`sched.rs`), executors (`executor/container.rs` = podman/docker CLI,
  `executor/process.rs` = dev only), GPU providers (`gpu/`), scratch retention + NAS archive (`retention.rs`).
- `crates/client` — REST client used by the CLI and MCP.
- `crates/mcp` — rmcp tool router; served over stdio (`mlq mcp serve`) and mounted at `/mcp` in the daemon.
- `crates/cli` (`mlq`) — user CLI; `mlq daemon` runs mlqd in-process.
- `web/` — Vite + React + Tailwind dashboard; `pnpm build` → `web/dist`, embedded with `--features mlq-daemon/embed-web`.
- `packaging/` — systemd unit, example config, PKGBUILD. `images/Containerfile` — default job image.

## Conventions
- Wire types shared by everything are in `crates/core/src/api.rs`; mirror changes in `web/src/api.ts`.
- Job state machine: queued/held → starting → (pulling) → running → succeeded/failed/cancelled/timeout.
  `JobState::is_active()` = occupying resources. Add new states to `store.rs` SQL `IN (...)` lists too.
- Never mount user-chosen host paths into containers; only the job's `work/`, `out/` and configured `mounts`.
- Results flow: `/out` on SSD → archived to `retention.archive_dir` at job end → SSD copy deleted by sweep
  only after a verified archive.
- Logs go to stderr (stdout is the MCP stdio channel).

## Commands
- `cargo test --workspace`; `cargo fmt --all`
- `make dev-daemon` (process executor + fake GPUs), then `MLQ_TOKEN=$(cat /tmp/mlq-dev/admin.token) mlq info`
- `make release` builds everything with the dashboard embedded.
