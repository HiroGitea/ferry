# ferry

**A small job ferry for ML experiments: Claude (or you) hands over a command, ferry runs it in
an isolated container on a GPU machine, and brings the results back.**

[中文说明](README.zh.md)

```
 Claude Code ─MCP─┐
 ferry CLI ───────┼─▶ ferryd (control node: queue, quotas, SQLite, dashboard, NAS archive)
 dashboard ───────┘        │ schedules onto
                           ├─▶ local node      podman run --cpus --memory --device nvidia.com/gpu=N
                           └─▶ agents (ferryd --agent, over tailscale)
                                    │
             datasets: NAS (HDD, read-only /data) ─▶ scratch: SSD (/work, /out) ─▶ results: .7z on the NAS
```

- **Isolation.** Every job is a Podman (or Docker) container that only sees its own `/work`,
  `/out` and the configured dataset mounts. CPU, memory, PIDs and GPUs are hard limits
  (cgroups + CDI). Nothing a job installs touches the host.
- **Tasks with save points.** A *task* is a long-lived environment; its *steps* run one after
  another. Each successful step is committed as a snapshot and becomes HEAD; a failed,
  cancelled or timed-out step rolls back to the last snapshot. Check out any snapshot to branch,
  pin the ones you care about. Optional *keep-warm* reuses the idle container for the next step.
- **Claude-native.** An MCP server (stdio or Streamable HTTP) with tools to inspect the cluster,
  run jobs synchronously with progress, create tasks, read result files, check GPU usage and
  restore archives.
- **Scheduler.** Priorities, fair share (decayed GPU-hours), per-user quotas, dependencies,
  arrays, time limits, resources reserved for people working directly on the host.
  Multiple machines: one control node plus agents.
- **netdata-style monitoring.** Per-second host, GPU and per-container (cgroup) metrics, stored
  in tiers (per-second pages, per-minute and per-hour min/max/sum/count) with automatic tier
  selection; dashboard with dozens of synchronised charts from 5 minutes to 30 days.
- **Data lifecycle.** Scratch on SSD, archived to the NAS as soon as a job ends and verified;
  expired workspaces are removed only after a successful archive. Closing a task writes a
  self-contained `.7z` (environment image, `/work`, `/out`, logs, metrics, manifest with
  checksums) that can be restored on any node.
- **Survives restarts.** Steps run detached inside their container and are re-attached when the
  daemon comes back.

Status: early. Developed and tested on Linux with rootful Podman and fake GPUs; real NVIDIA GPUs
via CDI and rootless Podman follow the documented setup but need more field testing.

## Install (Arch Linux)

```bash
sudo pacman -S podman 7zip nvidia-container-toolkit nvidia-utils rust pnpm nodejs
```

```bash
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
```

```bash
make release      # builds the dashboard and embeds it: target/release/{ferryd,ferry}
```

Or skip the Rust and Node toolchains and take prebuilt binaries (x86_64 / aarch64, glibc ≥ 2.28) from
[Releases](https://github.com/HiroGitea/ferry/releases); each tarball also carries `packaging/`:

```bash
tar xzf ferry-*-x86_64-unknown-linux-gnu.tar.gz && cd ferry-*-x86_64-unknown-linux-gnu
```

```bash
sudo useradd -r -m -d /var/lib/ferry -s /usr/bin/nologin ferry && sudo loginctl enable-linger ferry
```

```bash
sudo install -m755 target/release/ferryd target/release/ferry /usr/local/bin/
```

```bash
sudo mkdir -p /etc/ferry && sudo cp packaging/ferry.toml /etc/ferry/ferry.toml && sudo $EDITOR /etc/ferry/ferry.toml
```

```bash
sudo cp packaging/ferryd.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now ferryd
```

The first start creates an `admin` user; its token is in `/var/lib/ferry/admin.token`.
Add people with invite links (below) or `ferry user add <name>`.
A PKGBUILD is in `packaging/` (`cd packaging && makepkg -si`).

### Configuration highlights (`/etc/ferry/ferry.toml`)

| Key | Meaning |
|---|---|
| `data_dir`, `scratch_root` | Put on the SSD. Workspaces: `<scratch_root>/<user>/{jobs,tasks}/<id>/{work,out}` |
| `managed_gpus`, `reserve_cpus`, `reserve_mem` | What the scheduler may hand out; the rest stays free for interactive use |
| `[[mounts]]` | Datasets on the NAS, e.g. `host = "/mnt/nas/datasets"`, `container = "/data"`, read-only |
| `[retention]` | `archive_dir` on the NAS, `keep_days`, `max_scratch` |
| `[archive]` | `.7z` on task close, which snapshot images to include, compression level |
| `[container]` | `user`, `userns = "keep-id"` (the NFS export must allow that uid), network, `shm_size` |
| `[metrics]` | Collection interval and retention per tier |
| `[cluster]` | `node_name`, `node_token` for agents, `local_node = false` for a pure control node |
| `allowed_images` | Image allow-list; build the default image with `images/Containerfile` |

### More machines

Run the control node as above (set `cluster.node_token`). On every compute machine, over
tailscale or a private network:

```bash
sudo cp packaging/ferry-agent.toml /etc/ferry/ferry.toml && sudo $EDITOR /etc/ferry/ferry.toml
```

```bash
sudo cp packaging/ferry-agent.service /etc/systemd/system/ && sudo systemctl enable --now ferry-agent
```

`ferry node list` shows every node, its free resources and mounts. Jobs go to any node with room
(`--node` pins them); a task stays on the node where its environment lives. Traffic is plain HTTP
with a shared token, so keep it inside tailscale or a trusted LAN (or put a TLS proxy in front).

## Usage

### Users

An admin creates an invite link (dashboard → Users → Invite links, or the CLI); the new user opens it,
picks a name and gets their API token, which works for the CLI, the dashboard and Claude alike.

```bash
ferry invite create --uses 5 --expires 7d --note "lab"     # --name alice locks it to one name, --admin
```

```bash
ferry register 'http://server:7070/register?code=inv_...'  # the new user: creates the account and logs in
```

```bash
ferry invite list ; ferry invite rm <id> ; ferry token create --name laptop
```

Links are single-use by default, expire after 7 days (90 at most) and only their hash is stored. Existing
users log in with a token:

```bash
ferry login --url http://server:7070 --token ferry_...
```

### Notifications

Each user picks a webhook (ferry JSON, or the Feishu / DingTalk / WeCom / Slack bot formats) and/or a
Telegram chat in dashboard → Settings. Admins enable Telegram with `notify.telegram_bot_token`.

```bash
ferry notify set --webhook https://open.feishu.cn/open-apis/bot/v2/hook/... --format feishu --task-steps true
```

```bash
ferry notify test
```

### Jobs

```bash
ferry submit -c 8 -m 32G -g 1 -t 4h --upload ./src -- python src/train.py --lr 1e-3
```

```bash
ferry submit -g 1 --gpu-mem 40G -- python big.py   # only cards with >= 40 GB; small jobs get the small cards first
```

```bash
ferry run -g 1 -- python eval.py         # submit, follow the log, wait; exit code is the job's
```

```bash
ferry submit train.sh                    # "#FERRY -g 1 -m 16G" lines in the script are options
```

```bash
ferry queue -a ; ferry logs <id> -f ; ferry cancel <id> ; ferry result <id> --download ./out
```

### Tasks

```bash
ferry task create llm-eval -g 1 -m 32G --keep-warm 10m      # prints the task id, e.g. 3
```

```bash
ferry run --task 3 -- pip install -r requirements.txt        # success -> snapshot 1
```

```bash
ferry run --task 3 -- python train.py                        # starts from snapshot 1; a failure rolls back
```

```bash
ferry task show 3 ; ferry task checkout 3 1 ; ferry task close 3
```

### Archives

```text
task-3-llm-eval-20261105-050858.7z
├── manifest.json        task, steps, snapshot graph, images, sha256 of every file
├── README.txt           summary and manual restore instructions
├── steps.jsonl
├── env/snap-11.oci.tar  environment image (HEAD by default; pinned/all/none configurable)
├── work/  out/  logs/<step>.log
└── metrics/task.csv     resource usage over the task's lifetime
```

```bash
ferry archive list ; ferry archive show <name> ; ferry archive import <name> --node gpu1
```

```bash
ferry task export 3 --offload            # write the .7z, then free SSD space and snapshot images
```

### Monitoring

The dashboard (`http://server:7070`) has Monitor, Tasks, Jobs, Usage, Nodes & GPUs and Users
pages. From a terminal:

```bash
ferry metrics 'gpu.*' --since 1h
```

### Claude

Same machine (stdio):

```bash
claude mcp add --scope user ferry -- ferry mcp serve
```

Remote (Streamable HTTP):

```bash
claude mcp add --transport http ferry http://server:7070/mcp --header "Authorization: Bearer ferry_..."
```

Tools: `cluster_status`, `submit_job` (dry run by default), `run_job`, `list_jobs`, `get_job`,
`cancel_job`, `get_job_log`, `get_result`, `read_artifact`, `get_usage`, `create_task`,
`list_tasks`, `get_task`, `checkout_snapshot`, `close_task`, `export_task`, `list_archives`,
`import_archive`, `get_metrics`.

## Development

```bash
make dev-daemon          # runtime = "process" (no isolation) and two fake GPUs on 127.0.0.1:7070
```

```bash
cargo test --workspace
```

```bash
cd web && pnpm dev       # dashboard dev server, proxies the API
```

Releases are automated: CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests and the dashboard
build on every push; bumping `version` in `Cargo.toml` and pushing a matching tag publishes binaries:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Instead of pushing a tag you can also run *Actions → Release → Run workflow*, which tags the
commit as `v<version>` itself. Versions with a suffix (`0.2.0-rc.1`) become pre-releases. `make dist` builds the same tarball locally.

Layout: `crates/core` (types, pure scheduler), `crates/daemon` (`ferryd`: API, tasks, executors,
nodes, metrics, archives), `crates/client`, `crates/mcp`, `crates/cli` (`ferry`), `web/`
(React dashboard), `packaging/`, `images/`. See `CLAUDE.md` for conventions.

## Limitations

- Whole-GPU allocation only (no MIG / memory slicing).
- Snapshots are image layers: roughly 100 successful steps per chain before the overlay layer
  limit; check out an early snapshot or the base image to start a new chain.
- `/work` and `/out` live on the SSD and are not rolled back with snapshots.
- No TLS built in; agents and the API are meant to run inside tailscale or a trusted network.
- Archives include the full image chain (the base image too), so PyTorch-based archives are large;
  use `images = "none"` to archive data only.

## License

MIT
