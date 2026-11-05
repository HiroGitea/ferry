# ferry

**给机器学习实验用的"渡船"：Claude（或你）把命令交过来，ferry 在 GPU 机器上的隔离容器里跑完，再把结果送回去。**

[English](README.md)

```
 Claude Code ─MCP─┐
 ferry CLI ───────┼─▶ ferryd（控制节点：队列、配额、SQLite、面板、NAS 归档）
 面板 ────────────┘        │ 调度到
                           ├─▶ 本机节点     podman run --cpus --memory --device nvidia.com/gpu=N
                           └─▶ 计算节点 agent（ferryd --agent，经 tailscale）
                                    │
            数据集：NAS（HDD，只读挂到 /data）─▶ 临时目录：SSD（/work、/out）─▶ 结果：NAS 上的 .7z
```

- **隔离**：每个作业是一个 Podman（或 Docker）容器，只能看到自己的 `/work`、`/out` 和配置好的数据集目录。CPU、内存、进程数、GPU 都是硬限制（cgroup + CDI），作业装什么都不会碰到宿主机。
- **任务与存档**：*任务* 是一个长期环境，*步骤* 在里面依次运行。成功的步骤会自动 commit 成快照并成为 HEAD；失败、取消、超时的步骤回滚到上一个快照。可以 checkout 任意快照开分支、pin 住重要快照。可选"保温"：下一步直接复用空闲容器。
- **原生支持 Claude**：MCP 服务器（stdio 或 Streamable HTTP），可以查看集群、同步运行作业并推送进度、建任务、读结果文件、查 GPU 用量、恢复存档。
- **调度**：优先级、fair-share（衰减的 GPU 时）、每用户配额、依赖、作业数组、时间限制，为直接登录宿主机的人预留资源。多台机器：一个控制节点 + 若干 agent。
- **仿 netdata 的监控**：每秒采集宿主机、GPU、每个容器（cgroup）的指标，分层存储（逐秒页、逐分钟和逐小时的 min/max/sum/count），查询自动选层；面板几十张同步十字线的图，5 分钟到 30 天。
- **数据生命周期**：临时目录在 SSD，作业一结束就校验后归档到 NAS；SSD 上过期的工作区只有在归档成功后才删除。关闭任务时生成自包含的 `.7z`（环境镜像、`/work`、`/out`、日志、监控曲线、带 sha256 的清单），能在任意节点恢复。
- **重启不丢**：步骤在容器里以后台方式运行，守护进程重启后会重新接上。

状态：早期。在 Linux + rootful Podman + 假 GPU 上开发和测试；真实 NVIDIA GPU（CDI）和 rootless Podman 按文档配置，还需要更多实机验证。

## 安装（Arch Linux）

```bash
sudo pacman -S podman 7zip nvidia-container-toolkit nvidia-utils rust pnpm nodejs
```

```bash
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
```

```bash
make release      # 构建面板并嵌入二进制：target/release/{ferryd,ferry}
```

也可以不装 Rust 和 Node，直接从 [Releases](https://github.com/HiroGitea/ferry/releases) 下载预编译二进制（x86_64 / aarch64，glibc ≥ 2.28），压缩包里也带了 `packaging/`：

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

首次启动会创建 `admin` 用户，token 在 `/var/lib/ferry/admin.token`。其他人用邀请链接（见下文）或 `ferry user add <名字>` 加入。也可以用 `packaging/` 里的 PKGBUILD：`cd packaging && makepkg -si`。

### 配置要点（`/etc/ferry/ferry.toml`）

| 项 | 说明 |
|---|---|
| `data_dir`、`scratch_root` | 放 SSD。工作区：`<scratch_root>/<用户>/{jobs,tasks}/<id>/{work,out}` |
| `managed_gpus`、`reserve_cpus`、`reserve_mem` | 交给调度器的资源，其余留给直接登录的人 |
| `[[mounts]]` | NAS 数据集，如 `host = "/mnt/nas/datasets"`、`container = "/data"`、只读 |
| `[retention]` | NAS 上的 `archive_dir`、`keep_days`、`max_scratch` |
| `[archive]` | 关闭任务时生成 .7z、包含哪些快照镜像、压缩级别 |
| `[container]` | `user`、`userns = "keep-id"`（NFS 导出要允许这个 uid）、网络、`shm_size` |
| `[metrics]` | 采集间隔、各层保留时间 |
| `[cluster]` | `node_name`、给 agent 用的 `node_token`、`local_node = false` 表示纯控制节点 |
| `allowed_images` | 镜像白名单；默认镜像可用 `images/Containerfile` 构建 |

### 多台机器

控制节点按上面装（设置 `cluster.node_token`）。每台计算机器（tailscale 或内网）：

```bash
sudo cp packaging/ferry-agent.toml /etc/ferry/ferry.toml && sudo $EDITOR /etc/ferry/ferry.toml
```

```bash
sudo cp packaging/ferry-agent.service /etc/systemd/system/ && sudo systemctl enable --now ferry-agent
```

`ferry node list` 列出所有节点、空闲资源和挂载。作业会分到任何有空位的节点（`--node` 可指定）；任务固定在它的环境所在的节点。节点间是 HTTP + 共享 token，请放在 tailscale 或可信内网里（或者前面加 TLS 反代）。

## 使用

### 用户

管理员生成邀请链接（面板 Users → Invite links，或命令行），新用户打开链接、填用户名，拿到自己的 API token；命令行、面板和 Claude 都用这个 token。

```bash
ferry invite create --uses 5 --expires 7d --note "实验室"   # --name alice 锁定用户名，--admin 生成管理员
```

```bash
ferry register 'http://server:7070/register?code=inv_...'  # 新用户：注册并自动登录
```

```bash
ferry invite list ; ferry invite rm <id> ; ferry token create --name laptop
```

链接默认只能用一次、7 天过期（最长 90 天），数据库只存哈希。已有账号用 token 登录：

```bash
ferry login --url http://server:7070 --token ferry_...
```

### 通知

每个用户在面板 Settings 里填自己的 Webhook（ferry JSON，或飞书 / 钉钉 / 企业微信 / Slack 机器人格式）和/或 Telegram chat id。管理员在配置里设 `notify.telegram_bot_token` 启用 Telegram。

```bash
ferry notify set --webhook https://open.feishu.cn/open-apis/bot/v2/hook/... --format feishu --task-steps true
```

```bash
ferry notify test
```

### 作业

```bash
ferry submit -c 8 -m 32G -g 1 -t 4h --upload ./src -- python src/train.py --lr 1e-3
```

```bash
ferry submit -g 1 --gpu-mem 40G -- python big.py   # 只要显存 ≥ 40G 的卡；没写的作业优先拿小卡
```

```bash
ferry run -g 1 -- python eval.py         # 提交 + 跟随日志 + 等待，退出码与作业一致
```

```bash
ferry submit train.sh                    # 脚本里的 "#FERRY -g 1 -m 16G" 行会被当作参数
```

```bash
ferry queue -a ; ferry logs <id> -f ; ferry cancel <id> ; ferry result <id> --download ./out
```

### 多步骤任务

```bash
ferry task create llm-eval -g 1 -m 32G --keep-warm 10m      # 打印任务 id，例如 3
```

```bash
ferry run --task 3 -- pip install -r requirements.txt        # 成功 → 存档 1
```

```bash
ferry run --task 3 -- python train.py                        # 从存档 1 开始；失败会回滚
```

```bash
ferry task show 3 ; ferry task checkout 3 1 ; ferry task close 3
```

### 存档文件

```text
task-3-llm-eval-20261105-050858.7z
├── manifest.json        任务、步骤、存档关系、包含的镜像、每个文件的 sha256
├── README.txt           摘要 + 手工恢复方法
├── steps.jsonl
├── env/snap-11.oci.tar  环境镜像（默认只含 HEAD；可选 pinned / all / none）
├── work/  out/  logs/<步骤>.log
└── metrics/task.csv     整个生命周期的资源曲线
```

```bash
ferry archive list ; ferry archive show <名字> ; ferry archive import <名字> --node gpu1
```

```bash
ferry task export 3 --offload            # 写出 .7z 后释放 SSD 工作区和快照镜像
```

### 监控

面板（`http://server:7070`）有 Monitor、Tasks、Jobs、Usage、Nodes & GPUs、Users 几页。终端里：

```bash
ferry metrics 'gpu.*' --since 1h
```

### 接入 Claude

同一台机器（stdio）：

```bash
claude mcp add --scope user ferry -- ferry mcp serve
```

远程（Streamable HTTP）：

```bash
claude mcp add --transport http ferry http://server:7070/mcp --header "Authorization: Bearer ferry_..."
```

工具：`cluster_status`、`submit_job`（默认 dry-run）、`run_job`、`list_jobs`、`get_job`、`cancel_job`、`get_job_log`、`get_result`、`read_artifact`、`get_usage`、`create_task`、`list_tasks`、`get_task`、`checkout_snapshot`、`close_task`、`export_task`、`list_archives`、`import_archive`、`get_metrics`。

## 开发

```bash
make dev-daemon          # runtime = "process"（无隔离）+ 两块假 GPU，监听 127.0.0.1:7070
```

```bash
cargo test --workspace
```

```bash
cd web && pnpm dev       # 面板开发服务器，API 走代理
```

发布是自动的：每次推送 CI（`.github/workflows/ci.yml`）会跑 fmt、clippy、测试和面板构建；把 `Cargo.toml` 里的 `version` 改好，再推送同名 tag 就会发布二进制：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

不想推 tag 的话，也可以在 *Actions → Release → Run workflow* 手动运行，它会自己把当前提交打成 `v<版本号>`。带后缀的版本（`0.2.0-rc.1`）发布为预发布版。本地用 `make dist` 可以打出同样的压缩包。

目录：`crates/core`（类型、纯函数调度器）、`crates/daemon`（`ferryd`：API、任务、执行器、节点、监控、存档）、`crates/client`、`crates/mcp`、`crates/cli`（`ferry`）、`web/`（React 面板）、`packaging/`、`images/`。约定见 `CLAUDE.md`。

## 限制

- 只能整卡分配，没有 MIG / 显存切分。
- 存档是镜像层：同一条链大约 100 个成功步骤后会碰到 overlay 层数上限；checkout 早期存档或基础镜像重新开一条链。
- `/work` 和 `/out` 在 SSD 上，不随快照回滚。
- 没有内置 TLS；API 和 agent 应放在 tailscale 或可信网络里。
- 7z 存档包含完整镜像链（含基础镜像），PyTorch 镜像会很大；`images = "none"` 只存数据。

## 许可证

MIT
