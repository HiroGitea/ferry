# mlq

单机 GPU/CPU 作业调度器：每个作业跑在自己的容器里，Claude 可以通过 MCP 申请算力并取回结果，
宿主机环境不受影响。Rust 后端（守护进程 + CLI + REST + MCP），React 可观测面板。

```
Claude Code / 面板 / mlq CLI ──token──▶ mlqd (axum, SQLite)
                                          │ 排队 · 配额 · fair-share · GPU 分配
                                          ▼
                             podman run --cpus N --memory XG --device nvidia.com/gpu=i
                                 -v <ssd>/work:/work  -v <ssd>/out:/out  -v /mnt/nas/datasets:/data:ro
                                          │
                                 作业结束 → /out + 日志复制到 NAS 归档 → SSD 到期清理
```

## 功能

- **隔离**：作业 = 一个 Podman（或 Docker）容器，只挂载自己的 `/work`、`/out` 和配置的数据集目录；
  CPU、内存、PID 数、GPU 通过 cgroup/CDI 硬限制；用户只存在于 SQLite 里，不需要 Linux 账号。
- **调度**：优先级 → fair-share（指数衰减的 GPU 时）→ 提交时间；按用户的配额（同时运行的 GPU / 作业数 / 排队数）；
  作业依赖、作业数组、时间限制；为宿主机直接使用预留 CPU / 内存 / 指定 GPU。
- **数据生命周期**：数据集在 NAS（NFS，只读挂进容器）；作业目录在 SSD；作业一结束就把 `/out` 和日志
  **校验复制**到 NAS 归档目录；SSD 副本按天数或容量上限清理，**未归档成功的永不删除**；
  删除后 `mlq result` / MCP `read_artifact` / 面板自动改从归档读取。
- **MCP**：`mlq mcp serve`（stdio）和 `http://host:7070/mcp`（Streamable HTTP + Bearer token）两种接入；
  工具：`cluster_status`、`submit_job`（默认 dry-run）、`run_job`（同步等待 + 进度通知）、`list_jobs`、
  `get_job`、`cancel_job`、`get_job_log`、`get_result`、`read_artifact`、`get_usage`。
- **面板**：用量与配额（每用户 GPU 时、每日堆叠图、配额进度、fair-share）、作业（实时日志、产物下载、取消）、
  GPU 实时指标、用户管理；SSE 实时刷新，断线自动退化为轮询；浅色/深色。

## 安装（Arch Linux）

```bash
sudo pacman -S podman nvidia-container-toolkit nvidia-utils rust pnpm nodejs
```

```bash
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml   # 生成 GPU 的 CDI 描述
```

```bash
make release        # 构建前端并嵌入，产物在 target/release/{mlqd,mlq}
```

```bash
sudo useradd -r -m -d /var/lib/mlq -s /usr/bin/nologin mlq && sudo loginctl enable-linger mlq
```

```bash
sudo install -m755 target/release/mlqd target/release/mlq /usr/local/bin/
```

```bash
sudo mkdir -p /etc/mlq && sudo cp packaging/mlq.toml /etc/mlq/mlq.toml && sudo $EDITOR /etc/mlq/mlq.toml
```

```bash
sudo cp packaging/mlqd.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now mlqd
```

首次启动会创建 `admin` 用户，token 在 `/var/lib/mlq/admin.token`（0600）。也可以用 `cd packaging && makepkg -si` 打成 pacman 包。

### 配置要点（`/etc/mlq/mlq.toml`）

| 项 | 说明 |
|---|---|
| `data_dir` / `scratch_root` | 放 SSD。作业目录 `<scratch_root>/<用户>/jobs/<id>/{work,out,job.log}` |
| `managed_gpus`, `reserve_cpus`, `reserve_mem` | 交给调度器的资源；其余留给直接登录宿主机的人 |
| `[[mounts]]` | NAS 数据集：`host = "/mnt/nas/datasets"`, `container = "/data"`, `read_only = true` |
| `[retention]` | `archive_dir = "/mnt/nas/mlq-results"`，`keep_days`，`max_scratch` |
| `[container]` | `user`/`userns = "keep-id"`：容器内 uid 映射到宿主机的 `mlq` 账号，NFS 导出需允许该 uid 读 |
| `allowed_images` | 镜像白名单；默认镜像可用 `podman build -f images/Containerfile` 构建 |

## 使用

```bash
mlq login --url http://server:7070 --token mlq_...      # 保存到 ~/.config/mlq/config.toml
```

```bash
mlq submit -c 8 -m 32G -g 1 -t 4h --upload ./src -- python train.py --lr 1e-3
```

```bash
mlq run -g 1 -- python eval.py          # 提交 + 跟随日志 + 等待，退出码与作业一致
```

```bash
mlq submit train.sh                      # 脚本里的 "#MLQ --gpus 1 --mem 16G" 行会被解析为参数
```

```bash
mlq queue -a                              # 队列；mlq logs <id> -f；mlq cancel <id>
```

```bash
mlq result <id> --download ./out          # 产物（/out）列表与下载，SSD 清理后自动从归档取
```

管理员：`mlq user add alice`（打印其 token）、`mlq quota alice --gpus 2 --jobs 4`、`mlq usage -u all`、`mlq image pull <ref>`、`mlq sweep`。

### 接入 Claude

同一台机器（stdio）：

```bash
claude mcp add --scope user mlq -- mlq mcp serve
```

远程（Streamable HTTP）：

```bash
claude mcp add --transport http mlq http://server:7070/mcp --header "Authorization: Bearer mlq_..."
```

在容器里，代码在 `/work`，结果写到 `/out`，数据集在 `/data`。Claude 调 `run_job` 时会收到退出码、日志尾部和 `/out` 文件列表，再用 `read_artifact` 读取如 `metrics.json`。

## 开发

```bash
make dev-daemon      # runtime = "process"（无隔离）+ 两块假 GPU，监听 127.0.0.1:7070
```

```bash
cargo test --workspace
```

```bash
cd web && pnpm dev   # 面板开发服务器，API 代理到 127.0.0.1:7070
```

## 目前的限制

- 单机。数据模型里保留了 `node_id` 和 `Executor` 抽象，多节点需要增加一个在计算节点上运行的 agent 执行器。
- 整卡分配，不做显存切分（可在训练代码里用 `torch.cuda.set_per_process_memory_fraction`）。
- 面板和 API 之间没有 TLS；对外开放请放在反向代理后面。
