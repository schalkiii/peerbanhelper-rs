# PeerBanHelper-RS

> PeerBanHelper 的 Rust 忠实重写（faithful rewrite）。
> 自动封禁不受欢迎、吸血和异常的 BT 客户端（反吸血），支持 qBittorrent 等下载器与自定义规则。

本项目是对 [PBH-BTN/PeerBanHelper](https://github.com/PBH-BTN/PeerBanHelper)（Java 实现，v9.5.1 基线）的 Rust 重写，
在**保持外部行为、配置、规则语义与封禁决策一致**的前提下，显著降低常驻内存、CPU 占用、启动时间与发布体积。

- 上游基线版本：PeerBanHelper **v9.5.1**
- 许可证：**GPL-3.0**（衍生作品义务，与上游一致）
- 前端：**原样复用**上游 Vue3 + ArcoDesign WebUI（静态托管，不重写）
- 行为契约见 [SPEC.md](./SPEC.md)；进度、待办与已知差异见 [PLAN.md](./PLAN.md)

---

## 为什么用 Rust 重写

上游 Java 版每 5 秒执行一轮 ban wave：拉取 torrent 列表 → 逐 torrent 拉取 peers → 对「每个 peer × 每个规则模块」
`CompletableFuture.runAsync` 一个任务（数千 peer 时每轮产生数万个短命任务），叠加 Jackson 分配、JVM GC 与
固定的 JVM 基线开销。Rust 版用 tokio 异步 + 信号量限并发批量拉取 + serde 零成本反序列化 + 无 GC 重写该流水线。

**实测（Windows x64，release；实机数据来自与 Java v9.5.1 正式部署的同配置 dry-run 并行对跑）：**

| 指标 | Java v9.5.1 | Rust（本仓库） |
| --- | --- | --- |
| 常驻内存（空载） | 350–550 MB | **32.9 MB**（私有内存 10.5 MB） |
| 常驻内存 RSS（实机负载） | 780–851 MB | **36–104 MB**（≈ 1/8 ~ 1/20） |
| 冷启动到可服务 | 8–15 s | **0.57 s** |
| 发布体积 | 镜像 ~200 MB+ / 安装包 150–280 MB（含 JRE） | **单二进制 7.82 MB**（Linux x64，strip 后，含文案资源） |
| 线程模型 | 每 peer×模块大量短命任务 | tokio 协作任务，空载实测 22 线程 |
| 单轮 ban wave（实机对跑） | 中位 426 ms | 中位 4452 ms（见注） |

> **wave 耗时注**：对跑环境含一个不可达下载器。上游语义对网络失败**不进入冷却**（仅凭据错误与
> `login0` 外抛异常计数），Rust 忠实复刻后每轮恒定多付两次连接尝试（~4s）；Java 同窗口也出现
> 7–9s 尖峰。在线下载器的判定耗时两侧均为亚秒级；封禁集合一致性由 mock 双跑黄金测试锁定。
> 对跑与基准脚本：`prepare_live.ps1` / `live_dualrun.ps1` / `java_dualrun.ps1` / `rust_dualrun.ps1`。

---

## 功能现状

### 规则引擎与模块

- 六种匹配器（`STARTS_WITH` / `ENDS_WITH` / `CONTAINS` / `EQUALS` / `REGEX` / `LENGTH`），
  语义与上游 `RuleParser.matchRule` 一致（FALSE 短路优先、末条 TRUE 胜、DEFAULT 不命中）
- 全部上游规则模块均已实现并按 `registerModules()` 顺序接入：
  `ip-address-blocker`（CIDR/范围/端口 + GeoIP 四维度）、`peer-id-blacklist`、`client-name-blacklist`、
  `expression-engine`、`progress-cheat-blocker`、`multi-dialing-blocker`、`auto-range-ban`、
  `btn`、`ip-address-blocker-rules`（远程订阅 + sha256 缓存）、`anti-vampire`、`ptr-blacklist`、
  `idle-connection-dos-protection` 等；默认启用集合与上游 `profile.yml` 一致
- 多模块聚合按 `PeerAction` 等级 + 更长 ban 时长择优；模块结果携带上游 i18n 键，落库/展示按配置语言渲染

### 下载器

qBittorrent / Transmission（≥ 4.1.0）/ Deluge / BiglyBT / BitComet（≥ 2.18）/ Aria2Next 六个适配器，
覆盖登录与会话管理、torrent/peer 拉取过滤、增量与全量封禁、统计、能力标志（按版本判定）与限速接口
（`traffic-sliding-capping` 真正下发）。

### 封禁下发链路

ban wave 三段式（全部下载器判定 → 统一写封禁表 → 统一下发，对齐上游 `digestion/banPeer/updateDownloader`）、
到期自动解封、重复封禁触发全量重放、封禁列表 CIDR 重映射（`banlist-remapping`）、
连续登录失败冷却（对齐 `AbstractDownloader` 计数口径）、匿名 blocklist 端点
（`/blocklist/p2p-plain-format` / `/blocklist/ip` / `/blocklist/dat-emule`）。

### GeoIP

`ip-address-blocker` 的 ASN / 国家地区 / 城市 / 网络类型四维度（对齐 `IPDB` + `GeoCN1|2`）；
数据库按 `ip-database` 配置自动下载与更新（三镜像轮换 + XZ 解压 + 45 天周期 + 校验后原子替换），
缺失/损坏或 `pbh.forceDisableIPDB` 时四维度全部不命中。
GeoCN 省市区解析所用的行政区划表（`ok_data_level3.csv`）已内嵌于二进制（对齐上游 jar 资源），
ipdb 目录放置同名文件可覆盖（更新区划数据无需重新编译）。

### AutoSTUN

内置 NAT 地址翻译（TCP STUN + 静态映射表 + 后台刷新）；UDP NAT 类型探测（仅遥测展示）；
TCP 转发器与端口保活。`enabled: false`（默认）时严格直通、零网络请求。

### 监控与持久化

`active-monitoring`（流量日志、日流量阈值告警、滑动窗口限速）与 `peer-analyse-service.*`
（session-analyse / peer-recording / swarm-tracking）按上游定时间隔驱动，落点为 SQLite
（`alert` / `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm` /
`torrents` / `history` 等，schema 与字段对齐上游）；PCB 历史落库（dirty 回写、启动恢复、过期清理）。

### 告警推送

9 个渠道（PushPlus / ServerChan / SMTP / Telegram / Bark / PushDeer / Gotify / Ntfy / Webhook），
支持 `body-template`、自定义请求头与 Markdown 渲染；阈值告警与登录冷却告警走推送。

### BTN 网络

判定模块（五类规则 + 现代协议 IP 白/黑名单）+ 完整传输层（配置握手、协议版本校验、规则拉取、
`X-BTN-ContentVersion` 本地缓存、PoW captcha、按 ability 独立调度与重试）+ 上报能力
（`submit_bans` / `submit_swarm` / `submit_histories` / `heartbeat`（multi_if）/ `ip_query` / `reconfigure`，
数据源为 `history` / `tracked_swarm` / `peer_records` 表）+ 脚本规则（`btn.allow-script-execute`）。
未配置服务端时零网络请求；未注入规则时恒 `pass()`。

### 表达式脚本

`expression-engine` 以 rhai 执行 `<data>/scripts/*.av`；内置 **AviatorScript 兼容层**
（加载时自动翻译为 rhai），上游社区脚本原样运行，返回值语义对齐 `ScriptEngineManager.handleResult`。
迁移参考：[docs/expression-engine-migration.md](./docs/expression-engine-migration.md)。

### Web 后端

axum 服务：Token 鉴权、健康检查、封禁列表/日志/统计/图表 API、下载器与推送渠道热管理
（增删改即时生效）、手动封禁/解封、告警读写（dismiss / dismissAll / delete）、
规则订阅管理（`/api/sub/*`）、`/api/peer/{ip}/btnQuery`、实时日志（SSE，对齐上游弃用 WebSocket 后的现状）、
静态 WebUI 托管。

> 鉴权：`config.yml` 的 `server.token` 为空时，首次启动会随机生成并写回文件；空 token 下所有
> 鉴权 API 返回 `303 /init`（对齐上游未完成初始化向导时的行为），不会匿名放行。

---

## 与上游的已知差异

少量已知差异（均不改变封禁决策，或已按更安全方向处理）在
[PLAN.md「与上游差异的对齐项」](./PLAN.md)中逐条说明并跟踪，例如：
表达式引擎以 rhai 替代 AviatorScript 的语义边界、BTN 遗留协议上报快照的数据源、
GeoIP 更新进度以日志代替后台任务 UI 等。

---

## 快速开始

### 从源码构建

```bash
cargo build --release
# 产物：target/release/pbh（Windows 为 pbh.exe），单二进制，内嵌默认配置
```

### 运行

```bash
# 前台运行（无 GUI，等价于上游 nogui 模式）
pbh --data ./data

# 指定端口
pbh --data ./data --port 9898 --address 0.0.0.0

# dry-run（只读接入，不向下载器下发任何封禁；用于对跑与观测）
pbh --data ./data --dry-run
```

首次启动在 `data/` 生成 `config.yml`（字段对齐上游 config.yml / profile.yml），浏览器打开
`http://127.0.0.1:9898`。WebUI 静态资源放入 `data/static/`（复用上游 `webui/dist` 构建产物）。

### 测试

```bash
cargo test --workspace
```

黄金测试位于 `crates/pbh-golden/`（夹具在 `tests/fixtures/`）；`pbh-mockqb` 提供 mock qBittorrent
服务（对跑/基准夹具）。模块 × 层级的覆盖矩阵、mock 对跑用例清单与已知缺口见
[docs/TESTING.md](docs/TESTING.md)。

---

## 工作区结构

```
peerbanhelper-rs/
├── README.md                # 本文件（当前状态）
├── SPEC.md                  # 行为契约（忠实重写的行为规格）
├── PLAN.md                  # 进度、待办、已知差异与开发历史
├── CHANGELOG.md             # 变更日志
├── docs/                    # 专题文档（如 AviatorScript → rhai 迁移指南）
├── Cargo.toml               # workspace
└── crates/
    ├── pbh-core/            # 领域模型、规则引擎、规则模块、GeoIP、AutoSTUN、BTN、ban 决策
    ├── pbh-downloader/      # Downloader trait + qB / Transmission / Deluge / BiglyBT / BitComet / Aria2Next
    ├── pbh-db/              # SQLite 持久化（含 BTN 上报数据源）
    ├── pbh-web/             # axum HTTP/SSE 服务 + 静态托管
    ├── pbh-golden/          # 黄金测试与夹具
    ├── pbh-mockqb/          # mock qBittorrent 服务（对跑/基准夹具）
    └── pbh/                 # 二进制：配置加载 + ban wave 调度 + 监控模块宿主 + 组装
```

## 技术选型

| 关注点 | 选型 |
| --- | --- |
| 异步运行时 | tokio |
| HTTP 服务 | axum（静态托管为手写 handler，无 tower-http） |
| HTTP 客户端 | reqwest（rustls，cookie store；阻塞客户端仅用于独立线程的下载/上报任务） |
| 序列化 | serde / serde_json / serde_yaml |
| 脚本引擎 | rhai（AviatorScript 兼容翻译层） |
| 数据库 | rusqlite（bundled；MySQL/PostgreSQL 见 PLAN 扩展项） |
| GeoIP | maxminddb + lzma-rs（mmdb 更新） |
| IP 网络 | ipnet |
| 正则 | regex |
| 日志/错误 | tracing / anyhow / thiserror |

## 验证

- `cargo test --workspace`：**530+ 个测试全部通过**（单元 + 黄金测试 L1 匹配器 / L2 模块与配置 /
  L3 适配器 / L4 端到端 / 实机社区脚本判定）；`cargo clippy --workspace --all-targets -D warnings` 零警告
- 行为等价性：mock 双跑（同一夹具分别喂给 Java/Rust，封禁集合逐 IP diff）与实机 dry-run 并行对跑
  （同配置/同下载器/同 GeoIP 库）均已验证，脚本见仓库根目录 `*_dualrun.ps1`
- 真机冒烟：release 二进制启动后 `/health`、`/api/metrics/general`、`/api/downloaders` 正常返回；
  下载器不可达时 ban wave 记录错误并继续，进程不崩溃

## 忠实重写原则

1. **行为优先**：封禁决策必须与 Java 版逐 peer 一致；任何差异都视为缺陷并由黄金测试捕获。
2. **契约对齐**：配置字段、下载器 API 调用顺序与载荷、规则 JSON 语法、数据库语义对齐上游。
3. **只优化实现，不改变语义**：并发模型、数据结构可重写，但判定阈值、默认值、过滤条件不得擅改。
4. **黄金测试先行**：每个移植的模块/适配器先有录制夹具与期望决策，再写实现。
