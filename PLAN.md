# PLAN — PeerBanHelper-RS 重写计划与黄金测试方法论

## 1. 总策略

- **基线锁定**：以 Java v9.5.1 为行为基线，先冻结功能再追求对等。
- **黄金测试驱动（Golden-Test-First）**：每移植一个模块/适配器，先录制 Java 侧输入输出夹具，
  Rust 实现必须逐字节/逐决策复现，再继续下一个模块。
- **垂直切片**：先打通「qB 登录 → 拉 torrent → 拉 peers → 规则判定 → 封禁 → 落库 → API」一条端到端链路，
  再水平扩展下载器与模块。
- **前端复用**：不重写 Vue WebUI，仅做静态托管与 API 对齐。

## 2. 黄金测试方法论（保证“忠实重写”的核心机制）

### 2.1 夹具来源

1. **录制夹具**：对 qBittorrent 真实/模拟实例抓取 HTTP 响应 JSON（`torrents/info`、`sync/torrentPeers`、
   `app/buildInfo`、`app/version`），脱敏后存入 `crates/pbh-golden/tests/fixtures/`。
   上游仓库 `test/fakeqb.py` 提供了 fake qB 参考，夹具格式与其响应保持一致。
2. **规则夹具**：直接采用上游 `profile.yml` 的默认 `banned-peer-id` / `banned-client-name` 规则，
  保证默认规则集逐字一致。
3. **决策夹具**：每个场景一份 `expected.json`，记录每个 peer 的期望 `action / module / 命中规则`。

### 2.2 测试分层

| 层 | 内容 | 夹具 |
| --- | --- | --- |
| L1 匹配器 | 6 种 matcher 与 matchRule 的 FALSE 短路/TRUE 覆盖语义 | 内联用例 + 边界（大小写、正则整段匹配） |
| L2 模块 | 各规则模块对 peer 的判定 | `peers_*.json` + `expected.json` |
| L3 适配器 | qB JSON → 领域对象的解析与过滤 | 录制的 qB 响应 |
| L4 端到端 | 夹具下载器 → ban wave → 封禁集合/落库 | 全链路夹具 |
| L5 对照（后续） | 同一夹具同时喂给 Java/Rust，diff 封禁决策 | 双运行器 |

### 2.3 HTTP 抽象（确定性测试）

下载器依赖一个最小异步 HTTP trait（`HttpFetcher`），生产环境用 reqwest 实现，
测试环境用「按 URL 返回录制夹具」的内存实现，从而无需网络即可完整测试适配器与 ban wave。

### 2.4 验收口径

- L1–L4 全部通过且 `cargo test --workspace` 绿。
- 每个 `[GOLDEN]` 契约点至少一个夹具用例。
- 决策差异为零：expected 中每个 peer 的 action/module 必须完全一致。

## 3. 阶段计划

### Phase 0 — 骨架与基准（已完成）

- [x] workspace、crate 划分、文档（README/SPEC/PLAN）
- [x] 领域模型、PeerFlag 解析
- [x] 规则引擎 + 6 匹配器 + L1 黄金测试
- [x] HTTP trait 抽象

### Phase 1 — qB 端到端垂直切片（已完成）

- [x] qB 适配器（登录/会话/torrents/peers/封禁）+ L3 夹具
- [x] PeerId / ClientName / IP / ProgressCheat 模块 + L2 夹具
- [x] SQLite 持久化
- [x] ban wave 调度器
- [x] axum 服务（健康/封禁/日志/统计/静态托管）
- [x] L4 端到端黄金测试
- [x] `cargo build --release`、`cargo test` 全绿

> 验证记录（Windows x64，Rust 1.98.0）：`cargo test --workspace` 36 个测试全部通过
> （core 10 / db 3 / web 1 / golden L1 6、L2 9、L3 5、L4 2）；`cargo clippy --workspace --all-targets`
> 零警告；`cargo build --release` 通过，单二进制 6.28 MB；真机冒烟空载 WorkingSet 32.9 MB、
> time-to-health 0.57 s，下载器离线时 wave 容错不崩。

### Phase 1.5 — 与 Java 源码逐行对照的保真修复（已完成）

对照上游 v9.5.1 源码（`RuleParser` / `PeerFlag` / `ProgressCheatBlocker` / `DigestionSession` /
`DownloaderServerImpl` / `AbstractQbittorrent` / `profile.yml`）逐项复核，修复以下**会导致封禁决策
与上游不一致**的缺陷，并全部由黄金测试锁定：

- [x] `matchRule`：TRUE 应**覆盖**记录（末条 TRUE 胜），而非只记首个 TRUE
- [x] `LENGTH`：改为 Java `String.length()` 的 **UTF-16 码元数**
- [x] `STARTS_WITH`/`ENDS_WITH`/`CONTAINS`：改为 Unicode `toLowerCase(Locale.ROOT)`；
      `EQUALS` 改为 Java `equalsIgnoreCase` 语义
- [x] 默认规则集逐字逐序对齐 `profile.yml`（含 `CONTAINS -rn0.0.0` 的位置、
      `qbittorrent/3.3.15` 等方法差异、第 19 条 `\xde\xad__` 的真实字节）
- [x] PeerId/ClientName 模块握手前置条件：`握手中 && 标识为空` 才跳过
      （原实现「只要握手中就跳过」可被吸血客户端用 0 速度绕过）
- [x] PCB `fast-pcb-test-percentage` / `fast-pcb-test-block-duration`
      默认值对齐上游（0.1 / 15000ms，原实现默认关闭）
- [x] PCB IP 实体缓存键**不含端口**（含端口会重复累加上传增量 → 误判过量下载）
- [x] 多模块结果聚合：按 `PeerAction` 等级 + 更长 ban 时长择优，替换「首个命中模块即短路」
- [x] 封禁到期解封 + 全量/增量下发决策（有解封项必须走全量；无变化不请求下载器）
- [x] qB 登录复用已有会话（不再每轮重复 `POST /auth/login`）；
      API Key 认证要求 `>= 5.2.0`、口令认证 `>= 4.5.0`
- [x] qB `piece_size/pieces_have` 补全条件改为 `<= 0`；`statistics` 在 alltime 全 0 时报错
- [x] ban wave 调度改为 fixed-delay 语义（`MissedTickBehavior::Delay`）
- [x] **`profile.yml` 驱动流水线**：`check-interval` / `ban-duration` /
      `ignore-peers-from-addresses` / `module.*`（enabled、ban-duration、规则集、IP/端口列表、
      PCB 全部参数）；模块节缺失或缺少 `enabled: true` 视为禁用（对齐 `shouldModuleEnabled`）
- [x] 模块注册顺序对齐上游 `registerModules()`
      （`ip-address-blocker → peer-id-blacklist → client-name-blacklist → expression-engine → progress-cheat-blocker`）
- [x] 出厂 `default-config.yml` 与上游 `profile.yml` 等价，由 L2 黄金测试锁定逐字一致
- [x] 全量封禁列表 CIDR 重映射（`banlist-remapping`：IPv6 `/52` 默认开启、IPv4 默认关闭），
      并按下载器能力（`RANGE_BAN_IP`）决定是否生成网段
- [x] 下载器能力标志按版本判定（qB `RANGE_BAN_IP` 需 `>= 5.3.0` / `5.2.0-beta1`），
      标志名对齐 `DownloaderFeatureFlag`
- [x] 地址翻译：Teredo（默认关，改写 IP+端口）/ NAT64 / IPv4-mapped 归一
- [x] PCB 历史落库（`enable-persist`）：dirty 实体回写、启动恢复、8 小时过期清理
- [x] Basic Auth 改为「收到 401 后重试一次」，不再每个请求预置凭据
- [x] i18n：`TranslationComponent` + 内嵌上游 `lang/*`（en_us/zh_cn/zh_tw/fallback）+ `data/lang` 覆盖，
      `{}` 位置填充与 `DecimalFormat("0.00%")` 百分比格式；模块结果携带上游 `Lang` 键与参数
- [x] `auto-range-ban`（默认启用）：与已封禁地址同前缀的 peer 连锁封禁
- [x] `multi-dialing-blocker`（默认启用）：子网内不同 IP 数超容忍值即封禁 + 追猎窗口
- [x] `anti-vampire`（默认启用）：迅雷预设（做种禁全部版本，下载仅放行 0019）
- [x] `Pipeline` 与 wave 共享内存封禁表（`auto-range-ban` 读取），
      wave 调整为「全部下载器判定 → 统一写封禁表 → 统一下发」（对齐上游 digestion/banPeer/updateDownloader 三段）

- [x] `ip-address-blocker-rules`（默认启用）：规则文本解析（注释累积 / DAT 格式 / 前缀块覆盖）、
      最长前缀匹配、sha256 + 本地缓存的热更新与回退；应用层按 `check-interval` 周期拉取
      （启动立即执行一次），缓存写入 `<data>/sub/<ruleId>.txt`

- [x] `ptr-blacklist` / `idle-connection-dos-protection`（上游默认关闭，显式启用即注册；
      PTR 采用「应用层预热缓存 + 模块只读」的设计，见 SPEC §5.9）
- [x] **Transmission 适配器**（>= 4.1.0）：JSON-RPC + 409 会话握手、blocklist 指向 PBH 端点、
      `completedSize = sizeWhenDone * percentDone`、base64 peer_id → ISO-8859-1、
      peers 复用 `torrent-get` 响应、强制全量封禁路径；无 `RANGE_BAN_IP`
- [x] **封禁列表端点**：`/blocklist/p2p-plain-format`、`/blocklist/ip`、`/blocklist/dat-emule`
      （匿名访问，含 Transmission 空列表 workaround）

> 验证记录（2026-09-21，第五轮）：`cargo test --workspace` **151 个测试全部通过**；
> Transmission 端到端（本地 mock RPC）与 blocklist 端点冒烟（真实封禁数据 + curl）均通过：
> `p2p-plain-format` 输出 3 条（IPv4 单地址 + IPv6 精确地址 + IPv6 `/52` 网段），
> `dat-emule`/`ip` 格式与上游一致。

> 验证记录（2026-09-20，第三轮）：`cargo test --workspace` **114 个测试全部通过**；
> 规则订阅端到端冒烟（本地 HTTP 服务作为订阅源）：启动打印
> `已启用规则模块: …, ip-address-blocker-rules, anti-vampire`、
> `IP黑名单订阅规则 all-in-one 加载成功`、`IP 黑名单规则订阅：all-in-one=2 条`，
> 并按上游路径写入 `data/sub/all-in-one.txt` 缓存。

> 验证记录（2026-09-20，第二轮）：`cargo test --workspace` **100 个测试全部通过**
> （core 17 + 封禁表 4 + 重映射 7 + i18n 8 / db 3 / downloader 1 /
> golden L1 11、L2 19 + 额外模块 10 + profile 7、L3 11、L4 2）；
> `cargo clippy --workspace --all-targets -D warnings` 零警告；
> 冒烟：`已启用规则模块: ip-address-blocker, peer-id-blacklist, client-name-blacklist,
> progress-cheat-blocker, multi-dialing-blocker, auto-range-ban, anti-vampire`
> ——即上游 `profile.yml` 中默认启用且会实际封禁的模块已全部就位。

> 验证记录（2026-09-21，第六轮 · 忠实性同步 review）：派 4 个 `code-explorer` 子 agent 对照
> Java v9.5.1 基线逐模块审查已移植实现。**结论：决策核心路径忠实**。
> 仅发现 1 处真实保真缺陷——`ip_rule_list.rs` 的 `cover_with_prefix_block` 未向下对齐到网络边界，
> 导致非对齐 DAT 区间（如 `[1.2.3.5, 1.2.3.6]`）整行被丢弃、应封禁的网段漏封。
> 已修复（`IpNet` 前按前缀掩码对齐起始地址）并新增黄金测试
> `dat_range_is_prefixed_down_to_network_boundary` + 内联 `cover_with_prefix_block` 对齐用例。
> 子 agent 另报“DAT 字段被 trim、上游不 trim”的差异——核查 Java `IPBlackRuleList.parseRuleLine`
> 后确认系误报：IP 字段走 `IPAddressUtil.getIPAddress`（内部 trim），仅等级字段走
> `Integer.parseInt`（不 trim），当前 Rust 实现已忠实复刻。其余为无决策影响的 cosmetic 差异。
> 全量 `cargo test --workspace` 152 通过，`cargo clippy` 零警告。

### Phase 1.6 — 保真差距收口（已完成）

- [x] `expression-engine`（rhai 替代 AviatorScript，默认空脚本目录无封禁；自 2026-09-23 起加载时
      经 `avscript::transpile` 自动翻译，上游社区 `.av` 脚本原样运行，实机脚本已固化为黄金测试）
- [x] `ptr-blacklist`、`idle-connection-dos-protection`（两者上游默认关闭，已实现并接入流水线）
- [x] Web API 按请求 locale 渲染 + `rule`/`reason` 结构化存储（`ban_logs` 接受 `?locale=`，落库 `TranslationComponent` 按需本地化）
- [x] `btn`：判定模块 `BtnNetworkOnline` 已实现（五类规则 + 现代协议 IP 白/黑名单能力）并按上游
      `registerModules()` 顺序（`AutoRangeBan → BtnNetworkOnline → IPBlackRuleList`）接入流水线，
      `module.btn` 默认启用；传输层（握手 / abilities / PoW / 上报 / 心跳）已移植并接线（见 Phase 1.7 与 Phase 3.1）
- [x] `ip-address-blocker` 的 GeoIP 维度：ASN / 国家地区 ISO / 城市（GeoCN 中文写法）/ 网络类型四个维度，
      对齐 `IPBlackList#reloadConfig` + `IPDB`/`GeoCN1|2`；应用层按 `ip-database` 段加载
      `<data>/ipdb/geoip/{GeoIP-City,GeoIP-ASN,GeoCN}.mmdb`，失败时不注入 provider（四维度全不命中）
- [x] 非封禁模块：`active-monitoring` 与 `peer-analyse-service.{session-analyse,swarm-tracking,peer-recording}`
      已实现并按上游定时任务间隔驱动（见 Phase 1.7 的持久化缺口）
- [x] 告警推送：9 个渠道（PushPlus / ServerChan / SMTP / Telegram / Bark / PushDeer / Gotify / Ntfy / Webhook），
      支持上游 `body-template` 与自定义请求头
- [x] 内置 NAT（AutoSTUN）地址翻译：TCP STUN 客户端（RFC 5389 Binding Request）+ 映射表 +
      后台 5 秒刷新；`ip-remapping.auto-stun.enabled=false` 时严格直通（默认关闭）
- [x] 其余下载器：Deluge / BiglyBT / BitComet / Aria2Next（各自按上游登录门槛、能力标志与封禁载荷实现）

### Phase 1.7 — 本轮接线后的已知缺口（按影响排序，均非静默省略）

- [x] **监控数据持久化（缺口 #1 关闭）**：生产落点由内存实现改为 `pbh-db` 的 `DbMonitorSink`
      （与其余持久化共用同一个 `Database`）。表结构逐条对齐上游 SQLite 建表脚本
      `resources/db/migration/sqlite/V1_1__initial_sqlite.sql` 及 V1_3 / V1_4 增量迁移：
      `alert`（上游表名是**单数**；`create_at` / `read_at` / `level` / `identifier` / `title` /
      `content`，后两列存 `TranslationComponent` 的 JSON）、`torrents`（键 `info_hash`）、
      `traffic_journal_v3`（键 `(timestamp, downloader)`）、
      `peer_connection_metrics_track`（键 `(timeframe_at, downloader, torrent_id, address, port)`）、
      `peer_connection_metrics`（键 `(timeframe_at, downloader)`）、
      `peer_records`（键 `(address, torrent_id, downloader)`，V1_3 起不含 port ）、
      `tracked_swarm`（键 `(ip, port, info_hash, downloader)`）。
      语义对齐点：流量日志只抬升不回落且 `*_at_start` 永不更新；聚合查询闭区间、`SUM` 允许负值、
      单下载器逐行 `MAX(0, …)`；`saveAggregating` 覆盖写保留主键、合并写**与上游 `merge()` 一样
      漏掉 `local_not_interested`**；`peer_records` 逐列 CASE upsert（`first_time_seen` / `peer_geoip`
      永不更新、计数被重置时整值累加）；`delete_metrics_tracks` 逐条删（#1518 的 `SQLITE_TOOBIG`
      规避）；`remove_connection_metrics_before` 为 `<=`、`remove_peer_records_before` 为 `<`；
      `tracked_swarm` 启动时 `resetTable` 清空（数据随本次运行会话）。`peer_records.peer_geoip`
      由 sink 用注入的 `GeoIP` provider 填充（对齐上游在 DAO 内查 IP 库），无库时落 NULL。
      全部 DB 失败一律 log-and-continue（对齐上游 DAO 外层 `catch (Throwable) + log`）。
- [x] **监控 Web API（缺口 #1 关闭）**：`pbh-web` 新增 `/api/modules/swarm-tracking`
      （**裸** `{"trackedSwarmSize": N}`，对齐上游 `SwarmTrackingModule.handleWebAPI` 不套 `StdResp`）、
      `/api/modules/swarm-tracking/details`（`page` / `pageSize` + `orderBy=字段|asc|desc`，
      返回 `{page, size, total, results}`）、`/api/alerts`（对齐 `PBHAlertController.handleListing`：
      未读告警、`title`/`content` 按请求 locale 渲染）；三者都在 Token 鉴权之后（上游 `Role.USER_READ`）。
      告警读写端点 `PATCH /api/alert/{id}/dismiss`、`POST /api/alert/dismissAll`、`DELETE /api/alert/{id}`
      也已移植（`read_at` 正常落库）。
- [x] **BTN 网络传输**：`pbh-core::btn_transport`（≈ `BtnNetwork` + `AbstractBtnAbility`）已实现配置端点握手、
      协议版本校验（实现版本 20，遗留/现代 abilities 分支）、`X-BTN-ContentVersion` + 本地缓存、
      PoW captcha（Base64 challenge → 递增 nonce → 摘要前置零位）、按 `interval`/`random_initial_delay`
      的到期调度与 600s 重试节流；已在 `pbh/src/main.rs` 接线（`std::thread` + 阻塞客户端，5s tick，
      默认禁用 ⇒ 不构造客户端/不起线程/零请求）。缓存落 `pbh-db` 既有 `meta` 键值表（对齐上游 `metadataDao`）。
      abilities（submit_* / heartbeat / ip-query / reconfigure）全部构造并调度，上报数据源为
      `pbh-db::DbBtnSubmitSource`（`history` / `tracked_swarm` / `peer_records` 表；见 Phase 3.1）。
      未注入规则时模块恒 `pass()`、绝不封禁
- [x] **BTN 脚本规则**：`btn.allow-script-execute` 为 true 时会编译并执行 BTN 规则集里的
      `scriptRules`（上游 AviatorScript ⇒ 本移植 rhai，与 `expression_engine` 同一套注入变量与返回值语义）；
      上游默认 `false` ⇒ 不编译、不执行
- [x] **GeoIP 数据库自动更新**：`pbh-core::geoip_update` 已实现三镜像轮换（GitHub Releases →
      pbh-static.paulzzh.com → pbh-static.ghostchu.com）、XZ 解压（纯 Rust `lzma-rs`）、
      45 天 mtime 更新间隔、解压后 `validateMMDB` 再原子替换；已在 `pbh/src/main.rs` 的
      `GeoIpDb::load` **之前**接线（对齐上游 `IPDB` 构造函数「先 updateMMDB 再 loadMMDB」）。
      `auto-update: false`（默认）⇒ 严格 no-op（零网络请求）；`pbh.forceDisableIPDB` ⇒ 整体旁路
- [x] **AutoSTUN 次要能力**：`auto_stun_probe`（UDP NAT 类型探测，对齐 cdnbye/上游 `StunManager`，
      仅 WebUI/遥测展示、从不参与 ban 判定）+ `auto_stun_forwarder`（TCP 转发器 + 端口保活心跳 +
      友好回环映射绑定）已实现；`auto-stun.enabled: false`（默认）⇒ 严格 no-op（无 socket/无探测/无线程）
- [x] **上传限速下发**：`Downloader` trait 已暴露 `getSpeedLimiter()` / `setSpeedLimiter()`（bytes/s，<=0 为不限制），
      六个适配器（qBittorrent / Transmission / Deluge / BiglyBT / BitComet / Aria2Next）均按上游端点实现；
      `traffic-sliding-capping` 现已真正下发：`run_scheduled` 在 `on_tick` 算出新限速后调用 `set_speed_limiter`，
      `collect_traffic_stats` 取当前限速（不支持的下载器对齐上游 `getSpeedLimiter() == null` ⇒ 跳过）。
      上游默认关闭该功能，默认配置下无行为差异
- [x] **流量阈值告警的推送通道**：`active-monitoring` 的 `traffic-monitoring.daily` 超阈值告警
      除落 `alert` 表（`/api/alerts` 暴露）外，现已通过 `AlertManager::publish_alert(push=true, …)`
      走 `push:` 渠道推送（`MonitorHost::with_alert_manager` 接线，标题 `[PeerBanHelper/WARN] …`，
      与落库完全相同的 title/content 渲染路径；失败仅记日志不影响 tick）。
      已核对上游 6 个 `push=true` 调用点，本移植现存集合中仅此一类待补，现已完整复刻。
      上游默认 `traffic-monitoring.daily: -1`（禁用），默认配置下无行为差异。

> 验证记录（2026-09-21，监控持久化 + 监控 Web API）：`cargo test --workspace` **402 个测试全部通过**
> （pbh-core 179、pbh 39、pbh-db 18、pbh-web 7、pbh-downloader 76、黄金测试 83）；
> 新增测试：pbh-db 14 个（五张表各自的落库/查询/清理边界，含 `*_at_start` 不可变、`merge()` 漏列、
> 负增量重置、`<=` / `<` 边界、`ORDER BY id DESC LIMIT 1`、`orderBy` 白名单）+
> pbh-web 3 个端点测试（`/api/modules/swarm-tracking` 裸 JSON、`details` 分页与排序、
> `/api/alerts` 按 locale 渲染 + 无 Token 401）+ pbh 1 个链路测试（`DbMonitorSink` 下
> `onPeersRetrieved` → 定时 flush 真正写入 `peer_records` / `tracked_swarm` / `peer_connection_metrics`）；
> `cargo clippy --workspace --all-targets` 零新增警告（余下 6 条告警位于未改动的 pbh-downloader /
> pbh-core / pbh `push.rs`）。真机冒烟：`./target/debug/pbh --data …` 启动后库内出现 7 张新表
> （`alert` / `torrents` / `traffic_journal_v3` / `peer_connection_metrics(_track)` /
> `peer_records` / `tracked_swarm`）与上游唯一索引，`curl` 三个新端点返回预期 JSON
> （`{"trackedSwarmSize":0}`、`{page,size,total,results}`、`{success:true,data:[]}`）。

> 验证记录（2026-09-21，缺口④ 上传限速下发）：`cargo test --workspace` **417 个测试全部通过**
> （pbh-downloader 79：新增 3 个限速方法单测——aria2 `get/changeGlobalOption` 解析、biglybt `GET/POST /speedlimiter`、
> bitcomet `GET/SET_CONNECTION_CONFIG`；上游默认 `traffic-sliding-capping.enabled: false`，默认配置下无行为差异）；
> 六个适配器 `get_speed_limiter` / `set_speed_limiter` 按上游端点实现，pbh `monitor.rs` 在 `run_scheduled`
> 把 `on_tick` 算出的 `SpeedLimitChange` 下发到 `set_speed_limiter`，`collect_traffic_stats` 填当前限速快照；
> `cargo clippy --workspace --all-targets` 零告警。

> 验证记录（本轮接线）：`cargo test --workspace` **368 个测试全部通过**
> （pbh-core 163、pbh 38、pbh-db 4、pbh-web 4、pbh-downloader 76、黄金测试 83）；
> 接线点：`ip-database` → `build_pipeline_with_geo`、`module.btn` → `registerModules` 顺序、
> `ip-remapping.auto-stun` → `with_auto_stun`（仅 `enabled: true`）、
> `module.active-monitoring` / `peer-analyse-service.*` → `build_monitor_modules` + `MonitorHost`。
> 同时修复 `ip_rule_list::RuleIndex::build` 的移位越界 panic（`/32`、`/128` 规则会让整条订阅加载崩溃），
> 并新增 `/0`、`/32`、`/128` 与最长前缀的边界回归用例。

### Phase 2 — 下载器与模块对等（已完成）

- [x] 收口 Phase 1.6 的保真差距（模块配置接入、CIDR remap、能力标志、地址翻译、i18n、GeoIP）
- [x] Transmission（>=4.1.0，JSON-RPC + 409 会话握手 + blocklist 端点）
- [x] Deluge（插件 RPC：`rpc-url` 默认 `/json`，支持增量 `ban_ips` 与全量 `replace_blocklist`）
- [x] BiglyBT（插件：`Authorization: Bearer <token>`，增量 `POST /bans` / 全量 `PUT /bans`）
- [x] BitComet（v2.18+：BCAESTool 协议凭据加密，恒走整份替换 `ipfilter/upload`）
- [x] Aria2Next（实验性：JSON-RPC + `--rpc-secret` 双通道鉴权，恒走整份 `setBtPeerBlocklist`）
- [x] 模块：多拨追猎、自动连锁封禁、AntiVampire、PTR 黑名单、空闲连接 DoS 防护
- [x] 规则订阅（网络 IP 集 + sha256 缓存 + 周期刷新）
- [x] 每新增下载器/模块同步新增 L2/L3 夹具

### Phase 3 — 生态能力（部分完成）

- [x] GeoIP/ASN（maxminddb，GeoLite2/GeoCN），四维度接入 `ip-address-blocker`
- [x] **规则订阅 Web 后端（`/api/sub/*`）**：`SubModule` 实现（`RuleSubBackend`）已挂载到 `AppState.sub_module`，
      `GET/PUT/PATCH/DELETE /api/sub/rule[s]`、`/api/sub/rules/update`、`/api/sub/rule/{id}/update`、
      `/api/sub/logs`（分页）、`/api/sub/interval` 全部可用；增删改即时回写 `config.yml` 的
      `module.ip-address-blocker-rules` 段，`refresh_all` 成功落 `rule_sub_log` / `rule_sub_info`
      （对齐上游 `RuleSub*Service`）；未启用时 404 `RULE_SUB_MODULE_DISABLED`
- [x] **BTN 上报类能力（submit_* / heartbeat / ip_query / reconfigure）**：拉取类（rules /
      ip_allowlist / ip_denylist + PoW + 配置握手）与上报类全部实现；上报数据源由
      `pbh-db::DbBtnSubmitSource` 注入（`history` / `tracked_swarm` / `peer_records`），
      `GET /api/peer/{ip}/btnQuery` 已接 `BtnNetwork::query_ip`（规格见「Phase 3.1」）
- [x] 表达式规则（rhai 替代 AviatorScript + `avscript` 自动翻译层，上游社区脚本原样运行；
      黄金测试锁定实机脚本判定与 reason）— 引擎已实现，默认空目录无封禁
- [x] 告警推送：Webhook / Telegram / 邮件（lettre）/ 及其余 6 个渠道 —— 见 Phase 1.6
- [x] 实时日志推送（Java v9.5.1 **已弃用 WebSocket**，实际用 SSE `/api/logs/live`；Rust 以 SSE 对齐）
- [ ] MySQL/PostgreSQL（sqlx）—— Java 默认 sqlite/h2，与对跑及默认路径无关，属扩展项

### Phase 3.1 — BTN 上报能力移植（已实现，`btn.submit: true` 时生效）

> 背景（2026-09-22 调研）：部署 config 默认 `btn.submit: true`，Java 端 submit_* 上报能力
> 会被构造。已对照上游完整核对请求/调度语义，作为实现蓝图，如下。

**调度模型（对齐 `BtnNetwork.java`，每 ability 各自独立调度，不随 rules 轮询一起跑）**：
- 每个 ability 在 `load()` 里各自 `scheduleWithFixedDelay`；`interval` / `random_initial_delay` 各自独立。
- 初始延迟 `ThreadLocalRandom.nextLong(random_initial_delay)`，随后固定 `interval`（fixed-delay）。
- 失败无指数退避：非 2xx 记日志 + `setLastStatus(false)` + 中断本页分页；**游标只在成功后推进**
  （下轮从旧游标重读 → 隐式重试）。
- PoW：ability `pow_captcha=true` 时 `GET {pow}?type=<submit_bans|submit_swarm|submit_history|heartbeat|ip_query>`
  取 challenge → 求解 → 追加 `X-BTN-PowID` / `X-BTN-PowSolution`，失败不追加头（Rust `gather_and_solve_captcha` 已有）。
- 认证：`BTN-AppID/Secret`、`X-BTN-AppID/Secret`、`Authentication: Bearer <id>@<secret>`；匿名补
  `X-BTN-InstallationID`（`BtnNetwork::request` 已实现）。

| 能力（JSON key） | HTTP | 载荷 | Java 数据源 | Rust 数据源现状（已落地） |
|---|---|---|---|---|
| `submit_bans` | POST+gzip | `BtnBanPing{bans:[BtnBan…]}` | `history` 表 `id>cursor` 分页 100（游标 `BtnAbilitySubmitBans.cursor`）join `torrent` | 新增 `history` 表（对齐 V1_1 + V1_2 索引），`record_bans` 同步落库（`ban-for-disconnect` 跳过），`DbBtnSubmitSource` 按 `id > cursor` 分页 |
| `submit_swarm` | POST+gzip | `BtnSwarmPing{swarms:[BtnSwarm…]}` | `tracked_swarm` 表 `last_seen+id` 游标、`flushAll` 前置 | `DbBtnSubmitSource` 直读 `tracked_swarm`（二元游标 `last_time_seen >= x AND id > y`） |
| `submit_history` | POST+gzip | `LegacyBtnPeerHistoryPing{populate_time,peers:[…]}` | `peer_records` `getPendingSubmitPeerRecords(lastSubmitAt)` 分页 5000、游标 `btn.submithistory.timestamp` | `DbBtnSubmitSource` 直读 `peer_records` JOIN `torrents`（时间游标 + 30 天饱和） |
| `reconfigure` | GET configUrl | 无 | 解析 `ability.reconfigure.version` 差异 → 重新握手 | 纯 HTTP，零新数据依赖 |
| `heartbeat` | POST | `{"ifaddr":"default"}`（multi_if 多网卡） | 无 | 纯 HTTP，零新数据依赖 |
| `ip_query` | GET `endpoint?ip=` | 响应 `{color,labels,bans,swarms,traffic,torrents}` | 其他模块按需 `query(address)` | 纯 HTTP；`GET /api/peer/{ip}/btnQuery` 经 `SharedBtnNetwork` 调 `query_ip` |

**Rust 移植最小改动**（全部完成）：
1. `btn_transport.rs`：`BtnAbilityKind` 增 `SubmitBans/SubmitSwarm/SubmitHistory/Heartbeat/IpQuery/Reconfigure`；
   `apply_config_response` 按 `config.submit` 与 `is_implemented` 决定注册；`sync_due` 加分支。
2. 新增 `BtnSubmitSource` trait，注入 `BtnNetwork`；每能力构建 HTTP+gzip 请求与游标（存 `BtnMetadataStore`）。
   实现方 `pbh-db::DbBtnSubmitSource`（`history` / `tracked_swarm` / `peer_records` 三表），
   在 `pbh/src/main.rs` 接线（`with_submit_source`）；`rule_name` / `description` 按服务端语言渲染
   （对齐上游 `tlUI(component)`）。
3. `InfoHashUtil.getHashedIdentifier`：`sha256(lowercase + crc32(lowercase))`（新增 `crc32fast` 依赖）。
4. 遗留协议（`min < 20`）的 `legacy_peer_snapshot` / `legacy_ban_snapshot` 依赖 `DownloaderServer`
   内存快照，DB 数据源保持 trait 默认空实现（等价上游「无待提交数据」）。

> 验证口径：BTN 上报面向外部 `sparkle.pbh-btn.com`，无法本地对真实实例跑，故以**协议 golden 测试**
>（捕获请求体/头对齐 Java DTO 字段）+ 注入时钟驱动游标推进断言为主；不影响封禁决策。

### Phase 4 — 分发与打磨（待办）

- [ ] 插件系统评估（WASM/wasmtime）
- [~] 性能基准：Java/RSS 同机对照（RSS、单轮耗时、启动）——**已实测**（2026-09-22/23 实机 dry-run
      对跑：稳态 RSS 102MB vs 851MB≈1/8；单轮 wave 中位 2116ms vs 87ms，慢因已定位为不可达下载器
      的每轮重连并已修复对齐上游冷却策略）；剩余：CPU/启动耗时对照与 README 实测表回填
- [ ] 跨平台打包（Windows/macOS/Linux 单二进制、Docker scratch/Alpine 镜像）
- [ ] 与 Java 版并行灰度（L5 双跑 diff）

## 4. 风险与对策

| 风险 | 对策 |
| --- | --- |
| 上游无自动化测试，行为基线难锁定 | 以录制夹具 + 默认规则 + 源码阈值三重固化 |
| 正则/大小写等细微语义偏差 | L1 专项用例，REGEX 用 `\A(?:...)\z` 对齐 Java `Matcher.matches()` 整段匹配 |
| ProgressCheat 跨轮历史语义复杂 | 先移植状态机与持久化，再用多轮夹具回归 |
| 下载器 API 差异大 | Downloader trait 统一，按适配器逐个黄金测试 |
| 上游持续迭代 | 固定 v9.5.1 基线，差异通过夹具版本管理 |

## 5. Definition of Done（每个模块）

1. SPEC 中有对应契约；2. 至少一个黄金夹具且 L2/L3 通过；3. `cargo build`/`clippy`/`test` 全绿；
4. 默认值与阈值与上游一致；5. PLAN 勾选并在 README 状态表更新。
