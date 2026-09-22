# PeerBanHelper-RS

> PeerBanHelper 的 Rust 忠实重写（faithful rewrite）。
> 自动封禁不受欢迎、吸血和异常的 BT 客户端（反吸血），支持 qBittorrent 等下载器与自定义规则。

本项目是对 [PBH-BTN/PeerBanHelper](https://github.com/PBH-BTN/PeerBanHelper)（Java 实现，v9.5.1 基线）的 Rust 重写，
目标是在**保持外部行为、配置、规则语义与封禁决策完全一致**的前提下，显著降低常驻内存、CPU 占用、启动时间与发布体积。

- 上游基线版本：PeerBanHelper **v9.5.1**
- 许可证：**GPL-3.0**（衍生作品义务，与上游一致）
- 前端：**原样复用**上游 Vue3 + ArcoDesign WebUI（静态托管，不重写）

---

## 为什么用 Rust 重写

上游 Java 版每 5 秒执行一轮 ban wave：拉取 torrent 列表 → 逐 torrent 拉取 peers → 对「每个 peer × 每个规则模块」
`CompletableFuture.runAsync` 一个任务（数千 peer 时每轮产生数万个短命任务），叠加 Jackson 分配、JVM GC 与
固定的 JVM 基线开销。官方已通过 `SoftMaxHeapSize`、`UseStringDeduplication`、EcoQoS 等手段打补丁式优化。

Rust 版用 tokio 异步 + 信号量限并发批量拉取 + serde 零成本反序列化 + 无 GC 重写该流水线。

**实测（Windows x64，release，空载 1 个离线 qB 下载器）与设计估算：**

| 指标 | Java 现状 | Rust（本仓库） | 改善 |
| --- | --- | --- | --- |
| 常驻内存 WorkingSet | 350–550 MB | **实测 32.9 MB**（私有内存 10.5 MB） | ↓ 约 90% |
| 冷启动到可服务 | 8–15 s | **实测 0.57 s**（time-to-health） | ↓ 约 93–96% |
| 发布体积 | 镜像 ~200 MB+ / 安装包 150–280 MB（含 JRE） | **单二进制 7.82 MB（Linux x64，含文案资源）**（release，strip 后） | ↓ 约 95%+ |
| 线程模型 | 每 peer×模块大量短命 `CompletableFuture` 任务 | tokio 协作任务，空载实测 22 线程 | 显著降低调度/GC 压力 |
| 综合 CPU（高负载 ban wave） | 基线 | 设计估算为基线的 40–70% | ↓ 30–60%（**待对照基准实测**） |

> 内存、启动、体积为本机 release 实测；CPU 高负载改善与 Java 侧数值为设计估算/上游典型值，
> 需在相同规模（数千 peer）下用 criterion / 对照压测复核，见 PLAN.md「性能基准」。

---

## 当前实现状态

重写按阶段推进，进度见 [PLAN.md](./PLAN.md)，功能契约见 [SPEC.md](./SPEC.md)。

### 已完成（Phase 0 / Phase 1 核心）

- [x] 领域模型：`PeerAddress` / `PeerFlag`（libtorrent flags 解析）/ `Peer` / `Torrent`
- [x] 规则引擎：`STARTS_WITH` / `ENDS_WITH` / `CONTAINS` / `EQUALS` / `REGEX` / `LENGTH` 六种匹配器，
      完整复刻 `RuleParser.matchRule` 的 **FALSE 短路优先、TRUE 可被覆盖（末条 TRUE 胜）、DEFAULT 不命中** 语义，
      并对齐 Java 的 Unicode 大小写折叠、`equalsIgnoreCase` 与 UTF-16 长度口径
- [x] 规则模块（上游 `profile.yml` 中默认启用且会实际封禁的全部模块）：
  - [x] PeerId 黑名单（`peer-id-blacklist`，内置上游默认规则）
  - [x] ClientName 黑名单（`client-name-blacklist`，内置上游默认规则）
  - [x] IP 黑名单（`ip-address-blocker`，支持 CIDR/单 IP/范围/端口 + GeoIP 四维度）
  - [x] 虚假进度检查器（`progress-cheat-blocker`，含进度倒退与快速测试）
  - [x] 范围自动封禁（`auto-range-ban`，与已封禁地址同前缀连锁封禁）
  - [x] 多拨封禁（`multi-dialing-blocker`，子网 IP 数超限 + 追猎窗口）
  - [x] IP 规则订阅（`ip-address-blocker-rules`，远程规则文件 + sha256 缓存 + 周期刷新；含 `/api/sub/*` Web 管理、更新历史落库）
  - [x] 反吸血（`anti-vampire`，迅雷预设）
  - [x] 表达式规则（`expression-engine`，rhai 等价 AviatorScript；脚本取自 `<data>/scripts/*.av`，
        默认空目录 → 不产生任何封禁，返回值语义对齐上游 `ScriptEngineManager.handleResult`）
  - [x] BTN 网络在线规则（`btn`，五类规则 + 现代协议 IP 白/黑名单；按上游 `registerModules()`
        顺序位于 `auto-range-ban` 之后；传输层含规则拉取与 `submit_*` / 心跳等全部上报能力，
        上报数据源为 `history` / `tracked_swarm` / `peer_records` 表；
        未注入规则时恒 pass，`btn.enabled=false` 时不发任何请求）
- [x] **GeoIP 四维度**：`ip-address-blocker` 的 ASN / 国家地区 ISO / 城市 / 网络类型，
      对齐 `IPDB` + `GeoCN1|2`（逐字段覆盖式合并、CN/TW/HK/MO 回填、行政区划表前缀查询）；
      应用层按 `config.yml` 的 `ip-database` 段加载 `<data>/ipdb/geoip/*.mmdb`，
      数据库缺失/损坏或 `pbh.forceDisableIPDB` 时不注入 provider ⇒ 四维度全不命中
- [x] **非封禁监控模块**（不参与 peer 判定，由 ban wave 循环按上游定时任务间隔驱动）：
  - [x] `active-monitoring`：小时级流量日志、每日上行阈值告警、24 小时滑动窗口限速计算
  - [x] `peer-analyse-service.session-analyse`：按天聚合 peer 连接指标 + 保留期清理
  - [x] `peer-analyse-service.peer-recording`：peer 状态/会话/传输/偏移量记录 + 清理
  - [x] `peer-analyse-service.swarm-tracking`：本次运行会话内的 swarm 跟踪（重启即清空）
  - [x] **落点为 SQLite**（`pbh-db::DbMonitorSink`，与其余持久化共用同一个 `Database`）：
        五张表 `alert` / `traffic_journal_v3` / `peer_connection_metrics(_track)` /
        `peer_records` / `tracked_swarm`（+ `torrents`）逐条对齐上游建表脚本与
        `mapper/sqlite/*.xml`；`peer_records.peer_geoip` 由 sink 内查 IP 库填充；
        Web 侧新增 `/api/modules/swarm-tracking`、`/api/modules/swarm-tracking/details`、`/api/alerts`
- [x] **AutoSTUN 内置 NAT 地址翻译**：TCP STUN 客户端（RFC 5389 Binding Request /
  `XOR-MAPPED-ADDRESS`）+ 静态映射表 + 后台 5 秒刷新；`ip-remapping.auto-stun.enabled=false`
  （默认）时严格直通、不发起任何网络请求
- [x] **告警推送**：9 个渠道（PushPlus / ServerChan / SMTP / Telegram / Bark / PushDeer /
      Gotify / Ntfy / Webhook），支持 `body-template`、自定义请求头与 Markdown 渲染
- [x] 下载器抽象 `Downloader` trait（含 `getSpeedLimiter()` / `setSpeedLimiter()` 限速接口，六个适配器按上游端点实现，`traffic-sliding-capping` 现已真正下发）
- [x] **Transmission 适配器**（>= 4.1.0）：JSON-RPC + 409 会话握手、blocklist 指向 PBH 自身端点、
      `completedSize = sizeWhenDone * percentDone`、base64 `peer_id` → ISO-8859-1、
      peers 复用 `torrent-get` 响应、强制全量封禁路径（无 `RANGE_BAN_IP`）
- [x] **Deluge 适配器**（PBH-Adapter-Deluge 插件）：`rpc-url` 默认 `/json`、`paused` 启动暂停、
      增量 `ban_ips` 与全量 `replace_blocklist` 两条路径
- [x] **BiglyBT 适配器**（PBH-Adapter-BiglyBT 插件 >= 1.3.0）：`Authorization: Bearer <token>`、
      增量 `POST /bans` / 全量 `PUT /bans`、`ignore-private` 默认 false（与 qB 相反）
- [x] **BitComet 适配器**（>= 2.18）：WebUI 凭据按 BCAESTool 协议加密后登录，
      封禁恒为整份替换（`/api/config/ipfilter/upload`，`import_type=replace` + Base64 数据文件）
- [x] **Aria2Next 适配器**（仅 `aria2-next` 分支）：JSON-RPC + `--rpc-secret`
      （`Authorization: token:<secret>` 与 params 首元素双通道），封禁恒走 `setBtPeerBlocklist`
- [x] **封禁列表端点**（匿名，供下载器拉取）：`/blocklist/p2p-plain-format`（含 Transmission
      空列表 workaround）、`/blocklist/ip`、`/blocklist/dat-emule`
- [x] qBittorrent 适配器（忠实复刻）：
  - [x] Cookie 会话登录 `/auth/login`、`/app/buildInfo` 会话校验、`/app/version`
  - [x] `/torrents/info` 分页（`filter=active`、`limit/offset`、去重）
  - [x] `/sync/torrentPeers` 解析与过滤（HTTP/HTTPS/Web 连接、空 IP、`.onion`/`.i2p` 忽略）
  - [x] 增量封禁 `/transfer/banPeers`（`ip:port|ip:port`）
  - [x] 全量封禁 `/app/setPreferences`（`banned_IPs`）
  - [x] 握手 peer 判定（`up_speed<=0 && dl_speed<=0`）
- [x] SQLite 持久化：封禁日志、封禁列表、torrent/peer 快照、监控表
      （`alert` / `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` /
      `tracked_swarm` / `torrents`；schema 与字段对齐上游）
- [x] 内存封禁表：到期自动解封（`now > unbanAt`）、重复封禁触发全量重放、
      有解封项时强制全量下发封禁列表（对齐 `removeExpiredBans` + `setBanList`）
- [x] **`profile.yml` 驱动**：`check-interval` / `ban-duration` / `ignore-peers-from-addresses` /
      `module.*`（enabled、ban-duration、规则集、IP/端口、PCB 全部参数）；
      出厂默认配置与上游 `profile.yml` 等价（逐字由黄金测试锁定）
- [x] **i18n**：内嵌上游 `lang/*`（en_us/zh_cn/zh_tw/fallback）+ `data/lang` 覆盖，
      `TranslationComponent`（键+参数）、`{}` 位置填充、`DecimalFormat("0.00%")` 百分比
- [x] **地址重映射**：`banlist-remapping`（IPv6 `/52` 默认开）、Teredo/NAT64/IPv4-mapped 翻译，
      下载器能力标志按版本判定
- [x] **PCB 历史落库**（`enable-persist`）：dirty 回写、启动恢复、8 小时过期清理
- [x] ban wave 调度器（可配置 `check-interval`，默认 5000ms，信号量限并发，fixed-delay 语义）
- [x] axum Web 服务：健康检查、封禁列表/日志/统计 API、静态 WebUI 托管、Token 鉴权
- [x] **黄金测试（Golden Tests）**：录制的 qB 响应夹具 + 期望封禁决策，逐 peer 对齐上游行为

### 与上游逐行对照的保真修复（Phase 1.5）

对照 Java v9.5.1 源码复核后修复了一批**会改变封禁决策**的偏差（详见 PLAN.md Phase 1.5）：

- `matchRule` 末条 TRUE 胜出、`LENGTH` 用 UTF-16 长度、Unicode 大小写折叠
- 默认规则集逐字逐序对齐 `profile.yml`（含 `CONTAINS -rn0.0.0` 的位置、
  `qbittorrent/3.3.15` 的 `STARTS_WITH` 语义与第 19 条 `\xde\xad__` 的真实字节）
- PeerId/ClientName 握手前置条件：仅「握手中且标识为空」才跳过
- PCB 快速测试默认启用（0.1 / 15000ms）、IP 实体缓存键不含端口（避免重复累加导致误封）
- 多模块聚合按 `PeerAction` 等级 + 更长 ban 时长择优（不是首个模块短路）
- 全量封禁列表 CIDR 重映射（IPv6 `/52` 默认开、IPv4 默认关）、按下载器能力生成网段
- 下载器能力标志按版本判定（qB `RANGE_BAN_IP` 需 >= 5.3.0 / 5.2.0-beta1）
- 地址翻译：Teredo（默认关，改写 IP+端口）/ NAT64 / IPv4-mapped 归一
- PCB 历史落库（`enable-persist`）：dirty 回写、启动恢复、8 小时过期清理
- Basic Auth 仅在 401 后重试一次（不再预置凭据）
- **i18n**：内嵌上游 `lang/*` 文案表 + `TranslationComponent`，模块结果携带上游 `Lang` 键与参数，
  落库/日志按配置语言渲染（默认 `zh_cn`）；百分比格式对齐 `DecimalFormat("0.00%")`
- **补齐默认启用模块**：`auto-range-ban`、`multi-dialing-blocker`、`anti-vampire`
  （模块注册顺序、默认参数与文案键全部对齐上游），wave 调整为
  「全部下载器判定 → 统一写封禁表 → 统一下发」，与上游 digestion/banPeer/updateDownloader 三段一致

### 本轮接线（GeoIP / BTN / AutoSTUN / 监控模块）

四个新模块已从「实现完成」推进到「接入运行时」，接线点如下（默认配置与上游 `config.yml` /
`profile.yml` 逐字对齐，并由黄金测试锁定）：

| 能力 | 接线点 | 说明 |
| --- | --- | --- |
| GeoIP 四维度 | `config.yml` 的 `ip-database` 段 → `GeoIpDb::load(<data>/ipdb)` → `build_pipeline_with_geo(geo)` | 数据库不可用/`pbh.forceDisableIPDB` ⇒ 不注入 provider，四维度全不命中 |
| BTN | `profile.module.btn` → `build_pipeline_with_geo` 在 `auto-range-ban` 之后实例化 `BtnNetworkOnline` | 传输层已移植（握手/规则/上报/心跳，见上）；未注入规则时恒 `pass()` |
| AutoSTUN | `ip-remapping.auto-stun` → 仅 `enabled: true` 时 `AutoStunConfig::build()` + `with_auto_stun()` + 后台刷新线程 | `enabled: false`（默认）为**严格 no-op**，不发起任何网络请求 |
| 监控模块 | `profile.module.active-monitoring` / `peer-analyse-service.*` → `build_monitor_modules(sink)` → ban wave 循环按上游间隔驱动 | 落点为 `DbMonitorSink`（SQLite 五张监控表，与其余持久化共用同一个 `Database`）；启动时 `reset_tracked_swarm()`，`peer_records.peer_geoip` 由 sink 内查 IP 库填充 |

监控模块的调度对齐上游 `registerScheduledTask` 的 fixed-delay 语义（首次 delay 0 立即执行）：
`updateTrafficStatus` 每 1 分钟；`session-analyse` 的 `flushData` / `cleanup`、
`peer-recording` 的 `flush` / `cleanup`、`swarm-tracking` 的 `flushAll` 各按配置间隔；
`onPeersRetrieved` 在 wave 拉完每个 torrent 的 peers 后派发；退出时按各模块 `onDisable` 收尾刷写。

> **监控落库与监控 API（已接线）**：DB 版 `MonitorSink`（五张表 + `torrents`）与
> `/api/modules/swarm-tracking`、`/api/modules/swarm-tracking/details`、`/api/alerts`
> 均已实现；告警读写端点 `PATCH /api/alert/{id}/dismiss`、`POST /api/alert/dismissAll`、
> `DELETE /api/alert/{id}` 也已移植（`read_at` 正常落库），阈值告警的 `push:` 渠道推送已接线。

> **BTN 上报能力（已接线）**：`submit_bans` / `submit_swarm` / `submit_histories` /
> `heartbeat`（含 `multi_if` 多网卡）/ `ip_query` / `reconfigure`（服务端版本变更自动重新握手）
> 与遗留协议（`min < 20`）的 `submit_peers` / `submit_bans` 均已按上游 wire contract 移植。
> 现代协议上报数据由 `pbh-db::DbBtnSubmitSource` 注入（`history` / `tracked_swarm` /
> `peer_records` 表，分页游标与上游一致）；`GET /api/peer/{ip}/btnQuery` 经 `SharedBtnNetwork`
> 调 `BtnNetwork::query_ip`。`ban-for-disconnect` 不落 `history`（对齐 `PersistMetrics.recordPeerBan`）。
> 遗留协议快照依赖 `DownloaderServer` 内存数据（live peers / ban list），DB 数据源保持空实现。
> 本轮落地：BTN 上报数据源接线（`history` 表 + `DbBtnSubmitSource` + `btnQuery`）、
> GeoIP 数据库自动更新、AutoSTUN 的 UDP NAT 探测与 TCP 转发器、上传限速下发、
> 阈值告警的 `push:` 渠道推送。

### 路线图（后续阶段，见 PLAN.md / SPEC.md §9）

- [x] 规则订阅 `ip-address-blocker-rules`（上游默认启用，订阅远程 IP 规则文件）
- [x] 表达式规则 `expression-engine`（rhai 替代 AviatorScript，默认空脚本目录无封禁；
      逐行对照上游 `ExpressionRule` / `ScriptEngineManager.handleResult` 的返回值语义）；
      语法翻译表见 `docs/expression-engine-migration.md`（AviatorScript API → rhai 字段映射、语法对照、迁移示例）
- [x] `ptr-blacklist`、`idle-connection-dos-protection`（上游默认关闭，显式启用即生效）
- [x] `ip-address-blocker` 的 GeoIP 维度（ASN / 地区 / 城市 / 网络类型）
- [x] `btn`（BTN 网络在线规则判定模块，默认启用；传输层 + 脚本规则已接线）
- [x] 非封禁模块：`active-monitoring`、`peer-analyse-service.*`（SQLite 落点 + 监控 Web API）
- [x] 告警推送渠道（PushPlus / ServerChan / SMTP / Telegram / Bark / PushDeer / Gotify / Ntfy / Webhook）
- [x] 内置 NAT（AutoSTUN）地址翻译（默认关闭，严格直通）
- [x] 其余下载器：Deluge / BiglyBT / BitComet / Aria2Next（Transmission 见上）
- [x] 监控数据的 DB 持久化（`pbh-db::DbMonitorSink`：`alert` / `traffic_journal_v3` /
      `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm`）与监控 Web API
- [x] BTN 传输层（握手 / abilities / PoW / 缓存）与 BTN 脚本规则
- [x] GeoIP 数据库自动更新（mmdb 下载 + XZ 解压）
- [x] AutoSTUN 的 UDP NAT 类型探测、TCP 转发器与端口保活
- [x] Web 后端接线（`pbh-web` → `pbh::backend::PbhBackend`）：配置读写、下载器热管理
      （`/api/downloaders` 增删改 + 全量 reload）、手动封禁 / 解封（立即按 wave 下发）、
      推送渠道热管理（`/api/push` 增删改 + 即时生效）、告警读写（dismiss / dismissAll / delete）、
      实时日志环形缓冲与 WebSocket 推送
- [ ] Web API 按请求 locale 渲染、WebSocket 实时推送、MySQL/PostgreSQL、插件系统（WASM）

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
```

首次启动在 `data/` 生成 `config.yml`（字段对齐上游 config.yml / profile.yml），浏览器打开
`http://127.0.0.1:9898`。WebUI 静态资源可放入 `data/static/`（复用上游 `webui/dist` 构建产物）。

### 测试（含黄金测试）

```bash
cargo test --workspace
```

黄金测试位于 `crates/pbh-golden/`，夹具位于 `crates/pbh-golden/tests/fixtures/`。

---

## 工作区结构

```
peerbanhelper-rs/
├── README.md                # 本文件
├── SPEC.md                  # 功能契约（忠实重写的行为规格）
├── PLAN.md                  # 阶段计划、黄金测试方法论、进度
├── Cargo.toml               # workspace
└── crates/
    ├── pbh-core/            # 领域模型、规则引擎、规则模块、GeoIP、AutoSTUN、监控模块、ban 决策
    ├── pbh-downloader/      # Downloader trait + qB / Transmission / Deluge / BiglyBT / BitComet / Aria2Next
    ├── pbh-db/              # SQLite 持久化
    ├── pbh-web/             # axum HTTP/WS 服务 + 静态托管
    ├── pbh-golden/          # 黄金测试与夹具
    └── pbh/                 # 二进制：配置加载 + ban wave 调度 + 监控模块宿主 + 组装
```

## 技术选型

| 关注点 | 选型 |
| --- | --- |
| 异步运行时 | tokio |
| HTTP 服务 | axum（静态托管为手写 handler，无 tower-http） |
| HTTP 客户端 | reqwest（rustls，cookie store） |
| 序列化 | serde / serde_json / serde_yaml |
| 数据库 | rusqlite（bundled，后续 sqlx 支持 MySQL/PG） |
| 正则 | regex |
| IP 网络 | ipnet |
| 日志/错误 | tracing / anyhow / thiserror |

## 验证结果（Definition of Done）

- `cargo build --release --workspace`：通过（LTO + codegen-units=1 + strip），
  产物 `target/release/pbh` **7.82 MB**（Linux x64，含内嵌上游文案资源）；
  Windows x64 在 i18n 之前实测 6.28 MB。
- `cargo test --workspace`：**480+ 个测试全部通过**
  （pbh-core、pbh（推送渠道 + 监控宿主 + 出厂配置守卫）、pbh-db、pbh-web、
  pbh-downloader（qB / Transmission / Deluge / BiglyBT / BitComet / Aria2Next）、
  黄金测试（L1 匹配器 / L2 模块与 profile 配置 / L3 适配器 / L4 端到端））。
  各轮新增：`ip_rule_list` 的 `/0`、`/32`、`/128` 前缀边界回归（`RuleIndex::build` 的移位
  越界 panic 修复）、GeoIP/BTN/AutoSTUN/监控模块的配置与接线守卫、
  `MonitorHost` 的 `onPeersRetrieved` → 定时 flush 全链路（内存 sink）、
  Web 后端接线守卫（`PbhBackend` 热管理 / 手动封禁 / 推送渠道重建）。
- 规则订阅端到端冒烟（以本地 HTTP 服务作为订阅源）：`IP黑名单订阅规则 all-in-one 加载成功`、
  `IP 黑名单规则订阅：all-in-one=2 条`，并按上游路径写入 `data/sub/all-in-one.txt`。
- Transmission 端到端：mock RPC 覆盖 409 会话握手 / blocklist 配置与失败回退 / 种子与 peers 映射 /
  统计与能力标志（9 个用例）。
- blocklist 端点冒烟（真实封禁数据 + curl）：`p2p-plain-format` 输出 IPv4 单地址 + IPv6 精确地址 +
  IPv6 `/52` 网段三条，`dat-emule` / `ip` 格式与上游一致。
- `cargo clippy --workspace --all-targets -D warnings`：**零警告零错误**。
- 黄金测试 L4 端到端：录制 qB 夹具 → 登录 → torrents（私有种过滤）→ peers（HTTP/空IP/onion 过滤）→
  `default_pipeline` 判定 → 封禁集合与 `/transfer/banPeers` 载荷逐 peer 对齐上游（吸血 peerId、吸血 client、
  PCB 快速测试各一，局域网 bypass 跳过，正常 qB peer 不封）。
- 真机冒烟：release 二进制启动后 `/health`、`/api/metrics/general`、`/api/downloaders` 正常返回；
  下载器不可达时 ban wave 记录错误并继续，进程不崩溃；空载 WorkingSet 32.9 MB、time-to-health 0.57 s。

## 忠实重写原则

1. **行为优先**：封禁决策必须与 Java 版逐 peer 一致；任何差异都视为缺陷并由黄金测试捕获。
2. **契约对齐**：配置字段、qB API 调用顺序与载荷、规则 JSON 语法、数据库语义对齐上游。
3. **只优化实现，不改变语义**：并发模型、数据结构可重写，但判定阈值、默认值、过滤条件不得擅改。
4. **黄金测试先行**：每个移植的模块/适配器先有录制夹具与期望决策，再写实现。
