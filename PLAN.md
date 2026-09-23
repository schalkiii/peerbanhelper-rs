# PLAN — PeerBanHelper-RS 进度、待办与开发历史

> README 只描述当前状态；本文负责三件事：**已完成什么**、**接下来做什么**、**明确不做什么**，
> 并附录开发历史。行为契约见 [SPEC.md](./SPEC.md)，变更明细见 [CHANGELOG.md](./CHANGELOG.md)。

## 1. 方法论（黄金测试驱动）

- **基线锁定**：以 Java v9.5.1 为行为基线，先冻结功能再追求对等；上游持续迭代不追新。
- **黄金测试先行**：每移植一个模块/适配器，先录制 Java 侧输入输出夹具
  （qB 响应 JSON、上游默认规则、逐 peer 期望决策 `expected.json`），实现必须逐决策复现。
- **垂直切片**：先打通「登录 → 拉 torrent → 拉 peers → 判定 → 封禁 → 落库 → API」端到端链路，
  再水平扩展下载器与模块。
- **测试分层**：L1 匹配器 / L2 模块与配置 / L3 适配器解析 / L4 端到端 /
  L5 对照（同一夹具喂给 Java/Rust 做 diff：mock 双跑 + 实机 dry-run 并行对跑）。
- **确定性测试**：HTTP 依赖最小 trait（`HttpFetcher` / `BtnHttpClient` / `GeoIpHttpClient` 等），
  生产用 reqwest、测试用按 URL 回放夹具的内存实现，无需网络即可全链路测试。
- **验收口径**：`cargo test --workspace` 全绿；每个 `[GOLDEN]` 契约点至少一个夹具用例；
  决策差异为零；`cargo clippy --workspace --all-targets -D warnings` 零警告。

## 2. 当前状态基线

- `cargo test --workspace`：**530+ 个测试全部通过**（23 个 suite；覆盖 pbh-core / pbh / pbh-db /
  pbh-web / pbh-downloader / 黄金测试与实机社区脚本）。
- `cargo clippy --workspace --all-targets -D warnings`：零警告。
- 实机 dry-run 并行对跑（同配置/同下载器/同 GeoIP）：封禁集合一致；内存 36–104 MB vs Java 780–851 MB；
  单轮 wave 差异来自不可达下载器的每轮重连（上游语义忠实复刻），详见 README 实测表。

## 3. 已完成（按主题）

### 3.1 基础设施

- [x] workspace 与 crate 划分；领域模型（`PeerAddress` / `PeerFlag` libtorrent flags / `Peer` / `Torrent`）
- [x] 规则引擎：六种匹配器，完整对齐 `RuleParser.matchRule`（FALSE 短路、末条 TRUE 胜、DEFAULT 不命中）、
      Unicode 大小写折叠、`equalsIgnoreCase`、UTF-16 长度口径；默认规则集与上游 `profile.yml` 一致
- [x] HTTP trait 抽象与夹具回放测试设施；`profile.yml` 驱动流水线（模块开关/参数/规则集/bypass），
      模块节缺失或无 `enabled: true` 视为禁用

### 3.2 规则模块（全部上游模块）

- [x] `ip-address-blocker`：CIDR/单 IP/范围/端口 + GeoIP 四维度（ASN/地区/城市/网络类型，
      对齐 `IPDB` + `GeoCN1|2`）
- [x] `peer-id-blacklist` / `client-name-blacklist`（含握手前置条件：仅「握手中且标识为空」才跳过）
- [x] `progress-cheat-blocker`（进度倒退 + 快速测试；IP 实体缓存键不含端口；历史落库）
- [x] `auto-range-ban`（同前缀连锁）/ `multi-dialing-blocker`（子网超限 + 追猎窗口）/
      `anti-vampire`（迅雷预设：做种禁全部、下载放行 0019）
- [x] `ip-address-blocker-rules`：规则文本解析（注释累积 / DAT / 前缀块对齐）、最长前缀匹配、
      sha256 + 本地缓存热更新与回退、`/api/sub/*` Web 管理与更新历史落库
- [x] `expression-engine`：rhai 执行 + **AviatorScript 兼容层**（`avscript::transpile`，
      社区 `.av` 脚本原样运行，实机脚本固化为黄金测试）；迁移指南见 `docs/expression-engine-migration.md`
- [x] `btn`：五类规则 + 现代协议 IP 白/黑名单判定，注册顺序对齐上游
- [x] `ptr-blacklist` / `idle-connection-dos-protection`（上游默认关闭，显式启用即生效）
- [x] 多模块聚合按 `PeerAction` 等级 + 更长 ban 时长择优（非首个模块短路）

### 3.3 下载器（6 个适配器）

- [x] qBittorrent：Cookie 会话 + `buildInfo` 校验、`/torrents/info` 分页、`/sync/torrentPeers` 解析过滤、
      增量 `/transfer/banPeers` 与全量 `setPreferences`、API Key（≥5.2.0）/口令（≥4.5.0）门槛、
      能力标志按版本判定（`RANGE_BAN_IP` 需 ≥5.3.0 / 5.2.0-beta1）
- [x] Transmission（≥4.1.0）：JSON-RPC + 409 会话握手、blocklist 指向 PBH 端点、
      `completedSize = sizeWhenDone * percentDone`、base64 peer_id → ISO-8859-1
- [x] Deluge（插件）：`rpc-url` 默认 `/json`、增量 `ban_ips` 与全量 `replace_blocklist`
- [x] BiglyBT（插件 ≥1.3.0）：Bearer 认证、增量 `POST /bans` / 全量 `PUT /bans`
- [x] BitComet（≥2.18）：BCAESTool 凭据加密登录、恒走整份替换 `ipfilter/upload`
- [x] Aria2Next：JSON-RPC + `--rpc-secret` 双通道鉴权、恒走整份 `setBtPeerBlocklist`
- [x] 通用：登录冷却（`LoginGate`，计数口径逐分支对齐 `AbstractDownloader`）、
      `getSpeedLimiter` / `setSpeedLimiter` 限速接口（六个适配器按上游端点实现）、
      Basic Auth 仅 401 后重试一次

### 3.4 封禁下发链路

- [x] ban wave 三段式（判定 → 写表 → 下发，对齐 `digestion/banPeer/updateDownloader`）；
      下发阶段遍历**全部**下载器且下发前重新登录；fixed-delay 调度
- [x] 到期解封（`now > unbanAt`）、重复封禁全量重放、有解封强制全量；无变化不请求下载器
- [x] `banlist-remapping` CIDR 重映射（IPv6 `/52` 默认开、IPv4 默认关）+ IPv4-mapped IPv6 变体；
      地址翻译（内置 NAT → Teredo（默认关）→ NAT64 → IPv4-mapped 归一）
- [x] 匿名 blocklist 端点（`p2p-plain-format`（含 Transmission 空列表 workaround）/ `ip` / `dat-emule`）

### 3.5 GeoIP 数据库

- [x] 自动更新：三镜像轮换（GitHub Releases → pbh-static.paulzzh.com → pbh-static.ghostchu.com）、
      `.mmdb.xz` + 纯 Rust XZ 解压、45 天 mtime 周期、校验后原子替换；在 `GeoIpDb::load` 之前接线；
      `auto-update: false` 时已存在的库不覆盖、本地缺失的库补下一次（对齐上游 `needUpdateMMDB`）
- [x] 更新进度上屏：`BackgroundTaskRegistry` + SSE `GET /api/tasks/live`（逐字段对齐上游
      `PBHBackgroundTaskController` 的 DTO 与状态机），更新器经进度 sink 上报各阶段
- [x] 加载失败/`forceDisableIPDB` ⇒ 不注入 provider，四维度全不命中

### 3.6 AutoSTUN

- [x] 地址翻译：TCP STUN（RFC 5389 `XOR-MAPPED-ADDRESS`）+ 静态映射表 + 后台刷新；
      `enabled: false`（默认）严格直通
- [x] UDP NAT 类型探测（对齐 cdnbye/`StunManager`，仅遥测展示、不参与判定）
- [x] TCP 转发器 + 端口保活心跳 + 友好回环映射绑定（对齐 `connectToUpstreamFriendly`）

### 3.7 监控与持久化

- [x] `active-monitoring`（流量日志、日流量阈值告警 + push、滑动窗口限速计算与下发）与
      `peer-analyse-service.{session-analyse,peer-recording,swarm-tracking}`，按上游定时间隔驱动
- [x] `DbMonitorSink`：`alert` / `traffic_journal_v3` / `peer_connection_metrics(_track)` /
      `peer_records` / `tracked_swarm` / `torrents` / `history`，建表、索引与语义逐条对齐上游迁移脚本
      （`*_at_start` 不可变、聚合闭区间、CASE upsert、清理边界等）
- [x] 告警推送 9 渠道 + 阈值/冷却告警走 push（`publishAlert(push=true)` 语义）

### 3.8 BTN 网络

- [x] 传输层：配置端点握手、协议版本校验（实现版本 20，遗留/现代 abilities 分支）、
      `X-BTN-ContentVersion` + 本地缓存（`meta` 表 ≈ `metadataDao`）、PoW captcha、
      各 ability 独立调度（`interval` / `random_initial_delay`，游标成功后推进）与 600s 重试节流；
      `std::thread` + 阻塞客户端驱动，默认禁用 ⇒ 不起线程、零请求
- [x] 上报能力：`submit_bans`（`history` 表游标分页）/ `submit_swarm`（`tracked_swarm` 二元游标）/
      `submit_histories`（`peer_records` 时间游标 + 30 天饱和）/ `heartbeat`（multi_if）/
      `ip_query` / `reconfigure`；`ban-for-disconnect` 不落 `history`（对齐 `PersistMetrics`）
- [x] `GET /api/peer/{ip}/btnQuery` 经 `SharedBtnNetwork`；`btn.allow-script-execute` 脚本规则（rhai）

### 3.9 Web 后端

- [x] axum + Token 鉴权 + 静态 WebUI 托管；健康检查、封禁列表/日志/统计/图表 API
- [x] 配置读写、下载器热管理（增删改 + 全量 reload）、推送渠道热管理、手动封禁/解封（立即下发）
- [x] 告警读写（`dismiss` / `dismissAll` / `DELETE`，`read_at` 落库）、
      监控 API（`/api/modules/swarm-tracking(/details)`、`/api/alerts`）、规则订阅管理（`/api/sub/*`）
- [x] 实时日志：SSE `/api/logs/live`（对齐上游弃用 WebSocket 后的现状）+ 环形缓冲

### 3.10 工具链与验证

- [x] `pbh-mockqb`：mock qBittorrent 服务（对跑/基准夹具，含封禁下发录制）
- [x] 双跑与基准脚本：`java_dualrun.ps1` / `rust_dualrun.ps1` / `prepare_live.ps1` / `live_dualrun.ps1`；
      实测数据回填 README
- [x] 数据层对齐上游 + 部署布局兼容 + dry-run 模式；`PBH_DATA_DIR` 暴露给脚本目录解析

## 4. 计划与未完成（TODO）

### 4.1 与上游差异的对齐项

> 原则：有差异就写在这里，说明现状与上游行为，按影响排序逐项关闭。
> 当前**无未关闭项**（GeoIP 进度 UI 与 PTR 架构已对齐并移入 §3.5/§3.6）。

### 4.2 已知且保留的行为差异（说明，不计划「修复」）

- 表达式脚本 1500ms 超时兜底：上游 `maxScriptExecuteTime` 是死字段（声明后无引用），
  本移植保留该上限属**更严格**的防御性行为（只会把超时脚本判为 pass，方向安全）。
- 表达式脚本直接返回 `PeerAction` / `CheckResult` 对象：rhai 无对应类型，按 `pass()` 处理
  （上游会直接使用返回值）；实践中脚本均返回 bool / 数字 / 字符串，无实际差异。
- BTN 遗留协议封禁上报的 `ban_unique_id`：上游为 `sha256(metadata.toString())`（Java toString
  无法逐字节复现），本移植用同样唯一的封禁元数据 `random_id`（语义一致：去重键）。
- GeoIP 更新失败策略：上游「下载失败仍 move 空临时文件」会截断既有数据库，本移植失败时
  一律保留原文件（不复刻该缺陷）。
- 每日流量阈值告警等上游默认关闭的功能：默认配置下无行为差异，仅 `enabled: true` 时生效。

### 4.3 原生 GUI（Tauri 托盘壳）——方案规划

- [ ] **架构**：新增独立 crate `crates/pbh-gui`（**不加入 workspace members**——Tauri 在 Linux 需要
      webkit2gtk 系统库，纳入工作区会破坏 WSL/容器构建；Windows 侧单独构建）。
      职责：① 以子进程拉起 `pbh --data <dir>`（保留窗口关闭=隐藏到托盘、崩溃自动重启、日志重定向到文件）；
      ② 系统托盘（显示主窗口 / 打开数据目录 / 退出=结束子进程）；③ WebView 加载 `http://127.0.0.1:9898`
      （**复用上游 WebUI dist**，不在 Tauri 内重写前端，与「前端原样复用」原则一致）。
- [ ] **技术选型**：tauri v2（`tray-icon` feature，WebView2 系统组件）+ 单实例互斥（命名 Mutex +
      端口探测）；托盘图标内嵌（构建脚本生成 32×32 PNG/ICO）。
- [ ] **里程碑**：M1 托盘壳 + 子进程管理 + WebView 指向本地服务（Windows 先行）；
      M2 单实例与开机自启、托盘菜单完善；M3 Linux（libwebkit2gtk-4.1）/macOS 适配与打包。
- [ ] **验收**：关闭窗口仅隐藏；退出托盘菜单结束子进程并退出；`pbh.exe` 未就绪时窗口显示连接中提示并自动重载。

### 4.4 长时对跑基建（数十小时 ~ 数天）

- [x] **采样脚本 `longrun_sample.ps1`**：周期（默认 5 分钟）采样两侧进程 RSS/私有内存/CPU、
      SQLite 文件大小、`/health` 状态，追加 JSONL；Rust 侧崩溃自动拉起（对账游标存 DB，重启无损）。
- [x] **对账工具 `compare_dualrun`**（`pbh-db` 新增 bin）：读两侧 SQLite，比对 `history`
      （封禁历史：IP+端口+ban_at 时间窗匹配）、`alert` 数量与共享表行数摘要，差异输出 CSV + 控制台报告；
      容忍两侧表结构差异（先 PRAGMA 探测列）。**已增强**：IP 文本规范化（Java/Rust 的 IPv6
      映射写法差异不误报）与**命中模块集合比对**（同址同小时但模块无交集 = 真实行为差异）。
- [x] **在线比对不采用**（决策记录）：时钟漂移 + 重试时序差异会在天级产生大量假阳性；
      以离线 DB 对账为准，运行期只做健康采样。
- [x] 补齐（2026-09-23）：Java 侧数据库实测为 **SQLite**（`data/persist/peerbanhelper-nt.db`，
      魔数 `SQLite format 3`），无需 H2 导出；运行中经 `FileShare.ReadWrite` **共享读快照**
      （主库 + `-wal` + `-shm`）即可离线对账，已内置于 `longrun_sample.ps1`（周期 + 退出快照）；
      磁盘水位告警（剩余空间 / 单库体积阈值，JSONL 打 flag）；`pbh --tag` 身份标记
      （写入 `metadata.dualrun_tag` + `dualrun_started_at` + 启动日志）。

### 4.5 扩展项（超出 v9.5.1 对等范围）

- [ ] MySQL / PostgreSQL 支持（sqlx）——上游默认 sqlite/h2，属部署扩展
- [ ] 插件系统评估（WASM / wasmtime）
- [ ] 跨平台打包与发布：Windows/macOS 产物、Docker（scratch/Alpine）镜像、CI 自动发布

## 5. 计划不做（Non-goals）

- **不重写前端**：原样复用上游 Vue3 + ArcoDesign WebUI，仅静态托管与 API 对齐。
- **不移植上游死代码**：如 `peer-name-blocker-rules`（上游整个文件被注释且未注册）、
  WebSocket 实时推送（上游 v9.5.1 已弃用，实际走 SSE，已对齐）。
- **不复刻上游缺陷**：见 §4.2（GeoIP 更新截断、每轮重复登录等——Rust 侧按正确语义实现）。
- **不追上游新版本**：固定 v9.5.1 基线；上游后续行为变更按差异清单评估后再定。
- **不引入额外默认依赖**：默认部署只依赖 SQLite 与单二进制；MySQL/PG 等仅作为显式扩展项。

## 6. 风险与对策

| 风险 | 对策 |
| --- | --- |
| 上游无自动化测试，行为基线难锁定 | 录制夹具 + 默认规则 + 源码阈值三重固化；L5 双跑 diff |
| 正则/大小写等细微语义偏差 | L1 专项用例，REGEX 用 `\A(?:...)\z` 对齐 `Matcher.matches()` 整段匹配 |
| ProgressCheat 跨轮历史语义复杂 | 先移植状态机与持久化，再用多轮夹具回归 |
| 下载器 API 差异大 | Downloader trait 统一，按适配器逐个黄金测试 |
| 表达式脚本语义差异 | AviatorScript 兼容层 + 实机社区脚本黄金测试 + 迁移指南 |

## 7. Definition of Done（每个模块/适配器）

1. SPEC 中有对应契约；
2. 至少一个黄金夹具且对应分层通过；
3. `cargo build` / `clippy -D warnings` / `test` 全绿；
4. 默认值与阈值与上游一致；
5. 本文件「已完成」勾选 + CHANGELOG 记录。

## 8. 开发历史（里程碑）

> 变更明细见 [CHANGELOG.md](./CHANGELOG.md) 与 git log；此处仅保留里程碑脉络。

| 时间 | 里程碑 | 测试规模 |
| --- | --- | --- |
| 2026-09-20 | 骨架、规则引擎与 L1 黄金测试；qB 端到端垂直切片（L2/L3/L4）；SQLite、ban wave、axum 服务 | 36 → 114 |
| 2026-09-20/21 | 逐行保真修复（匹配器语义、聚合择优、下发决策、CIDR 重映射、i18n、地址翻译）；规则订阅/多拨/连锁/反吸血/ptr/idle 模块；Transmission 与 blocklist 端点 | 151 → 152 |
| 2026-09-21 | 表达式引擎（rhai）；其余下载器（Deluge/BiglyBT/BitComet/Aria2Next）；GeoIP 四维度；BTN 判定模块；监控模块与推送渠道；AutoSTUN 翻译 | 368 |
| 2026-09-21 | 监控 SQLite 持久化与监控 Web API；上传限速下发；GeoIP 自动更新、AutoSTUN 探测/转发、阈值告警推送；BTN 传输层与脚本规则；AviatorScript→rhai 迁移文档 | 417 → 480 |
| 2026-09-22 | 数据层对齐上游 + 部署布局兼容 + dry-run 对跑；BTN 上报数据源（history + DbBtnSubmitSource + btnQuery）；规则订阅 Web 后端；mockqb 与双跑脚本；L5 双跑修复（IPv4-mapped 变体） | — |
| 2026-09-23 | AviatorScript 兼容层（社区 `.av` 原样运行）；登录冷却计数口径重写；下发阶段对齐 `updateDownloader`；实机并行对跑与基准回填 | 530+ |
