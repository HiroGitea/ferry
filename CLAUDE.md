# ferry — developer notes

Containerised GPU/CPU job scheduler for a shared ML server. Rust workspace + React dashboard.

## Layout
- `crates/core` — domain types and the pure scheduler (`scheduler::schedule`). No IO. Unit tests live here.
- `crates/daemon` (`ferryd`) — axum REST/SSE API (`src/api/`), SQLite via sqlx (`store.rs`, `migrations/`),
  scheduler loop + job lifecycle (`sched.rs`), tasks = long-lived environments with steps, snapshots
  (`podman commit`), rollback and keep-warm (`tasks.rs`), executors (`executor/container.rs` = podman/docker CLI
  incl. env/exec/commit, `executor/process.rs` = dev only, `executor/usage.rs` = cgroup v1/v2 + /proc counters),
  GPU providers (`gpu/`), netdata-style metrics (`metrics/`: collector + tiered storage in `metrics.db`),
  scratch retention + NAS archive (`retention.rs`), self-contained .7z task archives with export/import/offload
  (`archive.rs`, needs the `7z` binary); nodes (`node.rs` registry + local node, `remote.rs` = agent client, `agent.rs` = `ferryd --agent` server, `api/nodes.rs` = registration/liveness).
- `crates/client` — REST client used by the CLI and MCP.
- `crates/mcp` — rmcp tool router; served over stdio (`ferry mcp serve`) and mounted at `/mcp` in the daemon.
- `crates/cli` (`ferry`) — user CLI; `ferry daemon` runs ferryd in-process.
- `web/` — Vite + React + Tailwind dashboard; `pnpm build` → `web/dist`, embedded with `--features ferry-daemon/embed-web`.
- `packaging/` — systemd unit, example config, PKGBUILD. `images/Containerfile` — default job image.

## Conventions
- Wire types shared by everything are in `crates/core/src/api.rs`; mirror changes in `web/src/api.ts`.
- Job state machine: queued/held → starting → (pulling) → running → succeeded/failed/cancelled/timeout.
  `JobState::is_active()` = occupying resources. Add new states to `store.rs` SQL `IN (...)` lists too.
- Task steps are jobs with `task_id`; the scheduler runs them one at a time per task. Success → commit snapshot
  (HEAD); anything else → remove the env container (rollback). Warm containers hold no scheduler resources.
- Metric keys: `host.*`, `gpu.<i>.*`, `sched.*`, `job.<id>.*`, `task.<id>.*`; job/task keys are visible to
  their owner and admins only (`api/metrics.rs::visible`).
- Auth: every `/api/v1` route needs a bearer token except `GET /api/v1/invite` and `POST /api/v1/register`
  (invite codes, `api/admin.rs`); registration is one transaction in `store.rs::register_with_invite`.
- Notifications: `notify.rs` listens for terminal `JobUpdated` events; other code calls `notify::send` (always delivered).
- Never mount user-chosen host paths into containers; only the job's `work/`, `out/` and configured `mounts`.
- Results flow: `/out` on SSD → archived to `retention.archive_dir` at job end → SSD copy deleted by sweep
  only after a verified archive.
- Logs go to stderr (stdout is the MCP stdio channel).

## Commands
- `cargo test --workspace`; `cargo fmt --all`
- `make dev-daemon` (process executor + fake GPUs; `gpu_provider = "fake:24G,48G"` gives mixed sizes), then `FERRY_TOKEN=$(cat /tmp/ferry-dev/admin.token) ferry info`
- Snapshots need a container runtime: set `runtime = "podman"` (rootful works for local testing).
- `make release` builds everything with the dashboard embedded; `make dist` packs it like a release asset.
- `make lint` = what CI enforces (`cargo fmt --check`, `clippy -D warnings`).
- Release: bump `version` in `Cargo.toml` (and `web/package.json`, `packaging/PKGBUILD`), push tag `vX.Y.Z` (or run the Release workflow manually);
  `.github/workflows/release.yml` cross-builds x86_64/aarch64 (cargo-zigbuild, glibc 2.28) and publishes.
