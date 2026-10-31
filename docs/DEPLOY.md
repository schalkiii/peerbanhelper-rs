# 生产部署：用 Rust 版（含原生 GUI）替换 Java 版

> 适用版本：本仓库 2026-10-25 之后的 main（含 PCB 共享 range、PTR 反查、ExternalSwitch、
> 代理/CORS/EcoQoS、下载器自动发现、live snapshot、`--ip-only` 对账工具与 Tauri GUI 壳）。
>
> 结论先行：**核心封禁判定链路已具备生产替代条件**，建议按「先并行观察 → 再切换」灰度迁移。

## 1. 前置条件

| 项 | Java 版 | Rust 版 | 说明 |
|---|---|---|---|
| 运行时 | 需 JRE | **无需**（单二进制） | 只需 `pbh.exe` + 可选 `pbh-gui.exe` |
| 数据目录 | `data/` | **直接复用** | 同 schema（SQLite），history/PCB/GeoIP 原样可用 |
| 配置 | `config/config.yml` + `profile.yml` | **兼容两种布局** | 上游分文件布局优先，单文件 `config.yml` 亦可 |
| WebUI | 内置 | `data/static/`（放上游 `webui/dist`） | 未提供时 WebUI 会提示「静态资源未安装」，不影响封禁 |
| GeoIP | 自动更新 | **三镜像自动更新**（官方 + 上游维护者镜像） | 与 fork 的 COS workflow 无关 |
| 自更新 | 有 | **无**（手动替换二进制） | 升级 = 停服 → 换 exe → 启动 |

## 2. 迁移步骤（推荐灰度）

### 步骤 0：备份

```powershell
Copy-Item -Recurse 'C:\CommonTools\PeerBanHelper\data' 'C:\CommonTools\PeerBanHelper\data.bak'
```

### 步骤 1：放置二进制与前端

> 生产目录约定为 **`C:\CommonTools\PeerBanHelper-rs`**（`scripts/deploy.ps1` 的目标）；
> 下文 `C:\PBH-RS` 只是示例路径，照抄时请替换。
> 日常发版优先用 `pwsh -File scripts/deploy.ps1`（构建 → 停服 → 复制 → health 验证），
> 仅在手工部署时走本节步骤。

```powershell
# 1) 从 GitHub Release 下载 zip 后先校验摘要（与 Release 资产页面显示的值比对）
Get-FileHash .\pbh-rs-<版本>-windows-x64.zip -Algorithm SHA256

# 2) 解压到目标目录（生产用 C:\CommonTools\PeerBanHelper-rs；C:\PBH-RS 仅为示例）
New-Item -ItemType Directory -Force -Path 'C:\PBH-RS'
Expand-Archive .\pbh-rs-<版本>-windows-x64.zip -DestinationPath 'C:\PBH-RS' -Force

# 3) 本地构建场景：直接用 target 产物
# pbh.exe（target\release\pbh.exe）与 pbh-gui.exe（crates\pbh-gui\target\release\pbh-gui.exe）
Copy-Item 'd:\workspace\peerbanhelper-rs\target\release\pbh.exe'      'C:\PBH-RS\'
Copy-Item 'd:\workspace\peerbanhelper-rs\crates\pbh-gui\target\release\pbh-gui.exe' 'C:\PBH-RS\'
# WebUI 前端（复用上游 webui/dist；从 Java 版安装目录或上游发行版复制）
Copy-Item -Recurse 'C:\CommonTools\PeerBanHelper\data\static' 'C:\PBH-RS\data\static' -ErrorAction SilentlyContinue
```

### 步骤 2：复用数据目录（两种做法，选一）

- **A（推荐，就地复用）**：直接把 Rust 指向原 `data` 目录——配置、封禁历史、PCB 状态、GeoIP 库全部保留；
- **B（先复制）**：把 `data` 复制到新目录再切换，保留 Java 侧可回退。

```powershell
# A：就地复用
$DATA = 'C:\CommonTools\PeerBanHelper\data'
# B：复制到新目录
# Copy-Item -Recurse 'C:\CommonTools\PeerBanHelper\data' 'C:\PBH-RS\data'; $DATA = 'C:\PBH-RS\data'
```

### 步骤 3：并行观察（可选但推荐）

保留 Java 版继续跑，另起一个 Rust 实例（**不同端口 + `--dry-run`**，只读接入、不向下载器下发封禁）：

```powershell
C:\PBH-RS\pbh.exe --data $DATA --port 9897 --dry-run
```

对照两侧日志与封禁判定（长跑对账工具见下）。

### 步骤 4：切换

1. 停止 Java 版（确保 `9898` 端口释放）；
2. 启动 Rust GUI（或命令行版）；

```powershell
# GUI 版（托盘壳；会自动拉起 pbh 子进程）
C:\PBH-RS\pbh-gui.exe --pbh-path 'C:\PBH-RS\pbh.exe' --data-dir $DATA --port 9898

# 或命令行版（无 GUI，适合服务器/无桌面会话）
C:\PBH-RS\pbh.exe --data $DATA --port 9898
```

> **GUI 参数必读**
> - `--pbh-path` 默认值是 **相对路径** `target/release/pbh.exe`（面向仓库布局），
>   **生产必须显式指定绝对路径**，否则拉不起子进程；
> - `--data-dir` 默认取 exe 同级 `../../data`，**生产必须显式指定**；
> - `--port` 默认 `9898`。
> - **附加模式**：若 `--port` 已有服务监听（例如 Java 版还开着），GUI 只开壳、
>   **不再拉起子进程**——所以必须先停 Java 再启动 GUI，否则 WebView 会连到旧服务。

### 步骤 5：验证

```powershell
# 产物完整性：解压后的 exe 应与发布包内一致；GUI 依赖的 WebView2Loader.dll 必须同级存在
Get-FileHash .\pbh.exe -Algorithm SHA256
Test-Path .\WebView2Loader.dll

# 健康检查
(Invoke-WebRequest 'http://127.0.0.1:9898/health' -UseBasicParsing).StatusCode   # 200

# 打开 WebUI
Start-Process 'http://127.0.0.1:9898'
```

验证清单：
- [ ] WebUI 可登录、下载器在线、能看到 peers 与封禁历史
- [ ] 新增封禁能下发到下载器（qB 的 ban 列表出现对应 IP）
- [ ] 日志无 `登录失败` / `下发失败` 连续报错
- [ ] 观察 24–72 小时：RSS/CPU 平稳，无冻结（见第 5 节已知风险）

### 步骤 6：回退（如有异常）

1. 关闭 Rust GUI / `pbh.exe`；
2. 启动 Java 版（数据目录未改动，可直接回退）；
3. Rust 运行期间新增的封禁保留在数据库，Java 版读取同一 schema 不受影响。

## 3. 无 GUI / 服务化部署（服务器场景）

GUI 适合桌面或家庭 NAS；服务器场景建议直接用 `pbh.exe` + 服务化：

- **Windows 服务**：用 NSSM / WinSW 包装
  `C:\PBH-RS\pbh.exe --data C:\PBH-RS\data --port 9898`；
- **任务计划程序**：触发器「计算机启动时」，操作同上，勾选「不管用户是否登录都要运行」；
- **Linux（systemd）**：`ExecStart=/opt/pbh/pbh --data /var/lib/pbh --port 9898`。

## 4. 长跑对账工具（迁移期判定一致性核对）

```bash
# 两侧 SQLite 的 history 差异（IP 粒度，消除端口伪差异）
cargo run -p pbh-db --bin compare_dualrun -- <java.db> <rust.db> --since 2026-10-05T16:21 --ip-only
```

- `--since` 用 UTC（`YYYY-MM-DDTHH:MM`），建议取 Rust 实例启动时刻；
- 差异归因模式见 [TESTING.md §4.1](TESTING.md)：级联前置缺失 / 订阅版本相位差 /
  rewind 观测相位差 / 双向相位差 / 级联路径差异，**均为观测干扰而非判定缺陷**。

## 5. 已知风险与缺口（切换前请知悉）

| 风险 | 说明 | 缓解 |
|---|---|---|
| **进程冻结**（pending-6）| 10.9 天长跑中出现 2 次冻结（8.5h 已修；36h 处新发：整进程 CPU 归零但 web 线程存活），minidump 已取证待分析 | 迁移初期**并行保留 Java 版**；生产建议加外部看门狗（检测 `/health` 与日志心跳，冻结则重启） |
| 自更新器缺失 | 需手动替换 exe | 升级窗口内短暂停服 |
| WebUI 前端 | 需手动放入 `data/static` | 一次部署 |
| BTN / 推送 / GeoIP | 已对齐上游 | — |
| IPv6/多拨段行为 | 判定一致（对跑验证） | — |

**看门狗**（已实现，`scripts/pbh-watchdog.ps1`）：探测 `/health` + **wave 完成心跳**
（读 `data/pbh-gui.log` 的「主循环返回」打点，停滞 > 300s 判定冻结并重启）。
**注意**：阈值必须 ≥ wave 间隔（config.yml 默认 120s）+ 启动初始化裕量；不要用进程
CPU 增量做心跳——wave 间隔内 main 线程空闲是常态，会周期性误杀（2026-10-07 教训）。
常驻方式见脚本头注释（任务计划程序 / NSSM）。

**日志轮转**：GUI 每 2s 检查一次 `data/pbh-gui.log`，超过 **16 MB** 就复制为
`pbh-gui.log.1` 并清空原文件（`.1 ~ .3` 依次后移，最旧的丢弃），总占用上限约 64 MB。
轮转后 `pbh-gui.log` 继续写入，看门狗读的还是这个文件，无需调整。
日志级别默认 `info,pbh=debug,reqwest=warn`；要更安静用 `RUST_LOG=info`，
要协议级细节用 `RUST_LOG=pbh=trace`（GUI 拉起的子进程继承环境变量）。
