# PeerBanHelper-RS

> PeerBanHelper 的 Rust 版本：功能、配置和封禁判定与 Java 版一致，但更省内存、启动更快、单个 exe 即用。
> 自动封禁不受欢迎、吸血和异常的 BT 对等体（反吸血），支持 qBittorrent 等下载器与自定义规则。

本项目是对 [PBH-BTN/PeerBanHelper](https://github.com/PBH-BTN/PeerBanHelper)（Java 原版）的 Rust 重写，
目标是**外部行为与 Java 版一致**（配置字段、规则语义、每个对等体的封禁决策都一样），在此前提下把
常驻内存、CPU 占用、启动时间和发布体积显著降下来。

- 对照的 Java 原版版本：**v9.5.3**（2026-10-28 已合并上游 v9.5.2 / v9.5.3 的改动）
- 本版本发布序列：`9.5.3-rs.3`（此前的 `9.5.1-rs.1 ~ rs.8` 对应 Java v9.5.1 基线）
- 许可证：**GPL-3.0**（与上游一致）
- 管理界面：**沿用上游 WebUI**（Vue3 + ArcoDesign，静态托管，不重写前端）
- 详细行为约定见 [SPEC.md](./SPEC.md)；进度、待办与已知差异见 [PLAN.md](./PLAN.md)

---

## 资源占用实测

Windows x64 release 构建的本机实测量值（2026-10-28，Windows 11 x64 / 32 核，版本 `9.5.3-rs.1`）。
「生产实跑」指本机长期运行的实例：2 个下载器在线、约 8 个活跃任务、已积累约 5000 条进度作弊历史记录、
加载了 GeoIP 数据库。

| 指标 | Java 原版 | Rust 版（本项目） |
| --- | --- | --- |
| 常驻内存（实跑，含 GeoIP 库映射） | 780–851 MB | **116 MB**（进程私有 98 MB） |
| 常驻内存（空载：无下载器、未加载 GeoIP） | 350–550 MB | **18 MB**（进程私有 6.8 MB） |
| CPU 占用（实跑，巡检间隔 2 分钟） | — | **平均 0.23 %** |
| 启动到可访问管理页面 | 8–15 s | **0.66 s**（实跑重启，含恢复数据库状态；空载 0.54 s） |
| 单轮巡检耗时（拉取 → 判定 → 下发） | — | **中位 61 ms**，九成低于 98 ms |
| 发布体积 | 安装包 150–280 MB（含 Java 运行时）/ Docker 镜像 ~200 MB | **`pbh.exe` 13.7 MB**；带托盘界面 `pbh-gui.exe` 6.7 MB；整包 zip 9.0 MB |

怎么自己复核这几个数字：

- 内存 / CPU / 线程：任务管理器或 `Get-Process pbh`，在程序稳定运行几分钟后读取。
- 启动时间：`pbh --data <新目录> --port 19899`，从进程启动到 `GET /health` 返回 200 的耗时。
- 单轮巡检耗时：日志每轮会输出两行——`wave#N 主循环返回，耗时 61.0ms`（DEBUG）与
  `已检查 1 个下载器，8 个活跃 Torrent 和 8 个 Peers；…（153ms）`（INFO）；
  守护脚本判活也依赖前者（详见 [docs/DEPLOY.md](docs/DEPLOY.md)）。
- 巡检耗时与下载器数量、任务数、网络往返强相关；上表只是本机当前负载下的观测值，
  不代表所有环境。Java 侧未在同一环境重测，所以这两行不留对比数字。

> **首次启动会联网下载 GeoIP 数据库**（约 1 分钟，取决于网络；Java 版同样如此）。
> 之后按配置周期自动更新（默认 45 天），库文件放在 `data/ipdb/`。

### 为什么能这么省

Java 原版每轮巡检会为「每个对等体 × 每条规则」单独提交一个异步任务，几千个对等体时单轮要抛出数万个
短命任务，加上 JSON 解析和垃圾回收的固定开销。Rust 版把这段流程改成**批量拉取 + 有界并发 + 无垃圾回收**，
判定顺序和结果不变，开销大幅下降。具体实现见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 功能现状

### 规则判定

- 六种匹配方式：`STARTS_WITH` / `ENDS_WITH` / `CONTAINS` / `EQUALS` / `REGEX` / `LENGTH`，
  短路优先级与判定结果与 Java 版逐条一致。
- Java 原版的规则模块全部实现并按原版顺序接入：IP 黑名单（含 GeoIP 的 ASN / 国家地区 / 城市 /
  网络类型四个维度）、对等体 ID 黑名单、客户端名黑名单、自定义脚本、进度作弊检查、多拨段检测、
  自动 IP 段封禁、BTN 网络、远程规则订阅、反吸血、PTR 黑名单、闲置连接保护等；
  默认启用哪些模块、默认阈值与上游配置文件逐字一致。
- 多个模块对同一个对等体给出不同结论时，按原版的优先级与更长的封禁时长合并；封禁原因按界面语言本地化显示。

### 下载器支持

qBittorrent / Transmission（≥ 4.1.0）/ Deluge / BiglyBT / BitComet（≥ 2.18，含 2.23 的新接口）/
Aria2Next。覆盖登录与会话维护、任务与对等体拉取过滤、增量与全量封禁下发、流量统计、
按版本自动判定的能力开关，以及限速接口的真实下发。

### 封禁下发

每轮巡检分三段执行（先全部下载器取数判定 → 统一写封禁表 → 统一下发），与原版一致；
封禁到期自动解除；重复封禁会触发一次全量重放；封禁列表可按 CIDR 重映射；
连续登录失败按原版口径进入冷却；同时提供匿名订阅地址
（`/blocklist/p2p-plain-format`、`/blocklist/ip`、`/blocklist/dat-emule`）供下载器直接拉取。

### GeoIP

IP 黑名单的 ASN / 国家地区 / 城市 / 网络类型四个维度依赖 GeoIP 数据库；程序按配置自动下载与更新
（三个镜像轮换、XZ 解压、45 天周期、校验后原子替换）。数据库缺失或损坏时，这四个维度一律不命中
（与原版 IP 库初始化失败时的降级行为一致）。中国省市区解析用的行政区划表已打进可执行文件，
在 `data/ipdb/` 放同名文件可覆盖，更新区划数据不必重新编译。

### NAT 穿透（AutoSTUN）

内置 NAT 地址翻译（TCP STUN + 静态映射表 + 后台刷新）、NAT 类型探测（仅用于界面展示与遥测）、
TCP 转发器与端口保活。默认关闭；关闭时完全不发起网络请求。

### 监控与数据落库

流量监控（流量流水、当日流量阈值告警、滑动窗口动态限速）、会话分析、对等体记录、
活跃种子跟踪，按原版设定的周期运行，数据写入 SQLite（表结构与字段与原版一致）。
进度作弊检查的历史记录会落库，重启后自动恢复，并按配置清理过期数据。

### 通知推送

9 个渠道（PushPlus / ServerChan / SMTP 邮件 / Telegram / Bark / PushDeer / Gotify / Ntfy / Webhook），
支持自定义消息模板、自定义请求头与 Markdown 渲染；阈值告警和登录冷却告警走同一通道。

### BTN 网络

判定部分（五类规则 + 新协议的 IP 白/黑名单）与完整的通讯层（配置握手、协议版本校验、规则拉取、
内容版本缓存、工作量证明验证、各能力独立调度与重试）均已实现，上报能力
（封禁、活跃种子、历史、心跳、IP 查询、重新配置）的数据源为本地数据库；
BTN 脚本规则受 `btn.allow-script-execute` 控制。未配置 BTN 服务时零网络请求。

### 自定义脚本

`expression-engine` 模块执行 `data/scripts/*.av`。脚本语言沿用上游的 AviatorScript 写法，
加载时自动翻译成内置脚本引擎执行，上游社区脚本可以直接放入；返回值语义与原版一致。
保存脚本时服务端会先做语法检查，不合法直接返回错误、不落盘（v9.5.2 起的行为）。
迁移说明：[docs/expression-engine-migration.md](./docs/expression-engine-migration.md)。

### 管理界面与接口

内置 HTTP 服务：Token 鉴权、健康检查、封禁列表 / 日志 / 统计 / 图表接口、下载器与推送渠道的增删改
（改完即时生效）、手动封禁与解封、告警处理、规则订阅管理、按 IP 查询对等体、实时日志（SSE）、
WebUI 静态托管。另外支持原版的 `lookup` / `resolvers` / `proxy` / `performance` 配置段、
域名反查（PTR）、本机下载器自动发现、可选放开跨域。**下载器详情页与活动对等体列表读取本轮巡检结果**，
不会为打开页面再访问一次下载器。

> 鉴权：配置里的 `server.token` 为空时，首次启动会随机生成并写回文件；
> token 尚未就绪时所有需要鉴权的接口返回 `303 → /init`（与原版未完成初始化向导时一致），不会匿名放行。

---

## 换成 Rust 版能得到什么

封禁行为与 Java 版一致（这是硬要求），额外带来四点实际好处：

### 更省资源，老设备也能跑

- 常驻内存降到 Java 版的约 **1/7**（实跑）到 **1/40**（空载）；NAS、软路由、小内存 VPS 不再吃紧
- 启动到能打开管理页面 **不到 1 秒**；一个 13.7 MB 的可执行文件，**不用装 Java**
- 数字与复现方式见上面的[资源占用实测](#资源占用实测)

### 安装简单，长期无人值守

- 下载解压即用。Windows 另有**带系统托盘的程序**：最小化进托盘、程序异常退出自动重启、
  打开即已登录，服务还没就绪时显示「正在启动」而不是报错页
- 托盘右键菜单：显示主窗口 / 在浏览器打开管理页面 / 查看实时日志 / **演练模式开关** /
  **重启服务**（约 2 秒，配置与数据无损）/ 退出
- 管理界面里没有侧边栏入口的**实时日志页**可从托盘直达（也可访问 `http://127.0.0.1:<端口>/log`）
- 界面里的外部链接自动交给**系统默认浏览器**打开，主窗口始终停留在管理界面
- 附带**一键部署脚本**与**守护脚本**（服务无响应时自动拉起），适合长期运行
- **演练模式**（照常监控和记录，但不向下载器下发封禁）可从托盘或设置页一键开关，便于先观察再启用

### 出问题时更容易查

- 巡检的每个阶段都有日志，万一卡住，**日志最后一行就是卡住的位置**
- 日志自动轮转：`data/pbh-gui.log` 超过 16 MB 就转存为 `.1`（保留 3 份，总计最多约 64 MB），
  长期运行不会把磁盘写满；需要更细的协议级日志时用 `RUST_LOG=pbh=trace` 启动
- 异常退出会记录退出原因，不用靠猜
- **691 个自动化测试**保证「每个对等体的封禁决策与 Java 版一致」，改动有回归保护

### 界面照常可用（沿用上游前端，问题都在服务端修）

- 修复登录后页面数据空白、BTN 状态误报「未启用」
- 状态页的内存总量 / 可用内存 / 浏览器 IP 正常显示
- 修复下载器列表增删改报错
- 修复「编辑下载器」弹窗打开后没有可编辑项（连接不上的下载器也能直接在界面改地址与凭据，
  不必再回去翻配置文件）
- 界面显示「0 / 空白 / NaN」这类字段对不上的问题建立了一套自动门禁（见 [docs/TESTING.md](docs/TESTING.md)）

---

## 与 Java 原版的已知差异

逐条记录在 [PLAN.md](./PLAN.md)，主要是实现方式不同而非判定结果不同，例如：

- 自定义脚本由内置的 rhai 引擎执行，而不是 Java 的 AviatorScript 引擎；个别写法需要小改（见迁移说明）。
- 代理设置通过进程环境变量生效（`proxy.setting`：`2` 显式覆盖，`0` / `1` 保留部署环境里已有的代理）；
  改代理需要重启，原版的 `pbh.downloader.bypassproxy`（让下载器绕过代理）在这种方式下不起作用。
- macOS 上没有可靠的本机监听端口枚举接口，下载器自动发现退化为只探测常用端口。
- GeoIP 更新进度以日志形式输出，未接入原版的后台任务进度界面。

封禁判定链路（含进度作弊检查在同前缀地址间共享计数）已与上游对齐。

---

## 快速开始

### 下载即用（Windows x64）

从 [Releases](https://github.com/schalkiii/peerbanhelper-rs/releases) 下载
`pbh-rs-<版本>-windows-x64.zip`，解压得到：

- `pbh.exe`：服务端（命令行或后台运行）
- `pbh-gui.exe` + `WebView2Loader.dll`：带托盘的界面程序（双击即可，会自动拉起服务端）

从 Java 版迁移、灰度并行观察与回退步骤见 [docs/DEPLOY.md](docs/DEPLOY.md)。

### 命令行运行

```bash
# 前台运行（无界面模式）
pbh --data ./data

# 指定端口与监听地址
pbh --data ./data --port 9898 --address 0.0.0.0

# 演练模式：照常监控和记录，但不向下载器下发封禁（用于先观察再启用）
pbh --data ./data --dry-run
```

首次启动会在 `data/` 生成 `config.yml`（字段与上游 `config.yml` / `profile.yml` 对齐），
然后浏览器打开 `http://127.0.0.1:9898`。管理界面的静态文件放在 `data/static/`
（可直接使用上游 `webui/dist` 的构建产物）。

### 从源码构建

```bash
cargo build --release
# 产物：target/release/pbh.exe（Windows）或 pbh（其他平台），单文件、内嵌默认配置
```

工具链固定为 `stable-x86_64-pc-windows-gnu`（见 `rust-toolchain.toml`）。
持续集成：`.github/workflows/ci.yml`（Windows runner，build + test + clippy）。

带托盘的界面程序是独立的构建工程（不混在主工程里，避免界面依赖污染命令行构建）：

```bash
cd crates/pbh-gui && cargo build --release
# 产物：crates/pbh-gui/target/release/pbh-gui.exe
```

用法：`pbh-gui [--pbh-path <pbh.exe>] [--data-dir <目录>] [--port 9898]`

- 以子进程方式拉起服务端（异常退出自动重启，日志写入 `data/pbh-gui.log`，超限自动轮转）；
  如果 `--port` 已有服务在监听，则只开界面不再重复拉起（便于接管已在运行的服务）。
- 托盘菜单：显示主窗口 / 在浏览器打开管理页面 / 退出（同时结束服务端）。关闭窗口即隐藏到托盘。
- 界面直接指向 `http://127.0.0.1:<端口>`，沿用上游 WebUI。

Windows 需要 WebView2 运行库（Win11 自带，Win10 需单独安装）；Linux 需要 `webkit2gtk`。

### 跑测试

```bash
cargo test --workspace
```

对照测试（以 Java 版实际输出为基准的回归测试）在 `crates/pbh-golden/`，夹具在 `tests/fixtures/`；
`pbh-mockqb` 提供模拟的 qBittorrent 服务，供对照与基准使用。
覆盖矩阵、对照用例清单与已知缺口见 [docs/TESTING.md](docs/TESTING.md)。

---

## 词汇对照

本 README 用中文说法，代码、日志和其余文档里常见的是下面这些简称：

| README 里的说法 | 代码 / 日志 / 上游文档里的说法 |
| --- | --- |
| 巡检（一轮巡检） | ban wave、`wave#N`、`check-interval` |
| 巡检结果快照 | live snapshot |
| 进度作弊检查 | PCB、`progress-cheat-blocker` |
| 演练模式 | dry-run、`--dry-run`、`server.dry-run` |
| 与 Java 版并行对比 | 对跑、双跑、dual-run |
| 以 Java 版输出为基准的回归测试 | 黄金测试、golden test |
| 分阶段日志 | phase 打点（`wave phase:N`） |
| 守护脚本 | 看门狗、watchdog |
| 无界面模式 | nogui |
| 域名反查 | PTR、`ptr-blacklist`、`lookup.dns-reverse-lookup` |
| 白名单地址 | bypass、`ignore-peers-from-addresses` |

---

## 目录结构

```
peerbanhelper-rs/
├── README.md                # 本文件（当前状态）
├── AGENTS.md                # AI 协作约定（工具链/部署/门禁速查）
├── SPEC.md                  # 行为契约（与 Java 版对齐的行为规格）
├── PLAN.md                  # 进度、待办、已知差异与开发历史
├── CHANGELOG.md             # 变更日志
├── docs/                    # 架构、API 索引、测试矩阵、部署与迁移指南、审计报告
├── .github/workflows/       # 持续集成（Windows + GNU 工具链）
├── scripts/                 # 一键部署、守护、发布产物校验
└── crates/
    ├── pbh-core/            # 模型、配置、规则引擎与模块、GeoIP、NAT 穿透、BTN、判定流水线
    ├── pbh-downloader/      # 各下载器适配（qB / Transmission / Deluge / BiglyBT / BitComet / Aria2Next）
    ├── pbh-db/              # SQLite 持久化（含 BTN 上报数据源）
    ├── pbh-web/             # HTTP 接口、实时日志、静态界面托管
    ├── pbh-golden/          # 以 Java 版输出为基准的回归测试与夹具
    ├── pbh-mockqb/          # 模拟 qBittorrent 服务（并行对比 / 基准用）
    └── pbh/                 # 命令行程序：读配置、装配模块、巡检调度、后台任务
```

## 技术选型

| 关注点 | 选型 |
| --- | --- |
| 异步运行时 | tokio |
| HTTP 服务 | axum（静态资源为手写处理，未引入 tower-http） |
| HTTP 客户端 | reqwest（rustls + cookie 存储；阻塞式客户端仅用于独立线程的下载/上报任务） |
| 序列化 | serde / serde_json / serde_yaml |
| 脚本引擎 | rhai（内置 AviatorScript 兼容翻译层） |
| 数据库 | rusqlite（内置 SQLite；MySQL / PostgreSQL 见 PLAN 扩展项） |
| GeoIP | maxminddb + lzma-rs（数据库更新） |
| IP 与网段处理 | ipnet |
| 正则 | regex |
| 日志与错误 | tracing / anyhow / thiserror |

## 验证方式

- `cargo test --workspace`：**691 个测试通过**（另有 1 个需要扫描本机端口的用例默认跳过）。
  覆盖层次：规则匹配 → 模块判定与配置 → 下载器适配 → 端到端链路 → 真实社区脚本判定。
  `cargo clippy --workspace --all-targets -D warnings` 零警告。
- 与 Java 版的行为等价性：用同一份模拟数据分别喂给两个版本，逐 IP 对比封禁集合；
  再在同机同配置同 GeoIP 的条件下用演练模式并行跑实机对比。脚本见仓库根目录 `*_dualrun.ps1`。
- 发布前冒烟：`/health`、统计与下载器接口正常；下载器不可达时记录错误并继续巡检，进程不崩溃。

## 重写原则

1. **行为优先**：封禁决策必须与 Java 版逐对等体一致；任何差异都当缺陷处理，由回归测试兜住。
2. **契约对齐**：配置字段、下载器接口调用顺序与载荷、规则语法、数据库语义都以上游为准。
3. **只优化实现，不改语义**：并发模型、数据结构可以重写，判定阈值、默认值、过滤条件不许动。
4. **测试先行**：每接入一个模块或下载器，先录制 Java 侧的输入输出夹具与期望结果，再写实现。
