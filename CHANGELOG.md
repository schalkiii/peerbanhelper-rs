# 变更日志（CHANGELOG）

本文件记录 peerbanhelper-rs 的功能新增、缺陷修复与行为变更，按时间倒序排列。
提交信息遵循 `type(scope): 中文描述` 约定。

## 未发布（working tree）

### feat(web,ptr): 后台任务 SSE 端点（GeoIP 进度上屏）+ PTR 解析所有权移入模块

- 对齐上游 `PBHBackgroundTaskController`：新增 `BackgroundTaskRegistry` 与 SSE `GET /api/tasks/live`
  （DTO/状态枚举/barType/locale 渲染逐字段一致）；GeoIP 更新器经 `with_progress_sink` 上报
  下载（按镜像 URL / 字节进度）/ 校验 / 写盘各阶段，`auto-update:false` 且库齐全 ⇒ 零任务零请求；
- `PtrBlacklist` 收编解析职责：`observe()` 入口 + 注入式 `PtrResolver`（std UDP 实现，读
  `/etc/resolv.conf`）、3 秒超时、负缓存 TTL/容量对齐上游 `ModuleMatchCache`；`check()` 只读不变；
- 忠实性评审修复（live_peers 生命周期）：改为**每轮整表清空**（对齐上游 `endSession` 整表替换，
  登录失败/已删除下载器的旧快照随之消失）；锁毒化统一 `into_inner()` 恢复策略；
  修复两个与本次改动无关的既有测试竞态（rulesub 临时目录、AutoSTUN 端口抢占）。

### feat(btn): 遗留协议上报快照落地（live peers + ban list 内存数据源）

- 新增 `pbh/src/btn_legacy.rs`：`LegacyAwareSubmitSource` 组合数据源——DB 批量数据
  （history / swarm / peer_records）委托 `DbBtnSubmitSource`，遗留协议
  `legacy_peer_snapshot` / `legacy_ban_snapshot` 来自内存；
- live peers：wave 每下载器轮次开始清空旧快照、每个 torrent 拉取成功后写入
  （对齐上游 `DownloaderServer` 每轮覆盖 livePeers）；
- 封禁快照对齐 `LegacyBtnAbilitySubmitBans.generateBans`：跳过 `ban_for_disconnect` /
  `exclude_from_report`、按 peer 去重、`btn_ban` 按 context 判定、`rule` 渲染 description；
  `ban_unique_id` 用元数据 `random_id`（上游 sha256(toString) 无法逐字节复现，见 PLAN §4.2）；
- 表达式引擎补 `peer.handshaking` getter（对齐上游 `Peer.isHandshaking()`）。

### fix(wave): 下发阶段对齐上游 `updateDownloader`（全部下载器 + 下发前重新登录）

- 本轮有新增封禁或解封时，对**全部**下载器下发（此前只对判定阶段成功的下载器下发）；
  判定阶段登录失败的下载器会在下发阶段**重新登录**，有机会补上封禁列表
  （对齐上游 `downloaderManager.stream().map(dl -> updateDownloader(dl, …))` 的全量遍历）；
- 下发前重新登录（每轮第二次 `login()`，经同一 LoginGate 计数，对齐上游语义），
  失败仅记日志并跳过（PAUSED 静默）；
- dry-run 仍在登录前短路（不下发任何请求）。

### fix(script): 忠实性审查修复（表达式引擎 + 登录冷却计数口径）

对本轮新增的 AviatorScript 兼容层与登录冷却机制做全面源码级审查（对照上游
`AbstractDownloader` / `DownloaderLoginResult` / `AVScriptEngine` / `BtnNetworkOnline` 等），
修复以下真实缺陷：

**表达式引擎 / 翻译器**
- 浮点返回值改为向零截断（对齐上游 `number.intValue()`）：此前 `round()` 会把
  `return 0.9` 误判为 BAN、`return 1.5` 误判为 SKIP；
- 裸赋值 `x = …` **一律**改写为 `let x = …`：Aviator 语句分隔符可省略（换行即分隔），
  按「语句起始位置」判定会漏 `let` → rhai 运行期未声明变量 → 整脚本静默 pass（漏封）；
- 注册跨类型 `+`（String+任意值 / 任意值+String）：Aviator `'下载=' + downloaded` 是合法拼接，
  rhai 缺省报运行期错 → 整脚本 pass（漏封）；
- 补齐类型转换内建 `double()` / `long()` / `int()` / `str()`（对齐上游
  `upload_ratio_check.av` 的 `double(uploaded) / double(downloaded)`）；
- BTN 脚本注入变量对齐 `BtnNetworkOnline`：`ramStorage` → `kvStorage`；脚本展示名改为
  解析内容里的 `## @NAME`（缺省回退规则集 key）；`parse_metadata` 移入 avscript 共享。

**登录冷却（LoginGate）计数口径——重写**
- `LoginResult` 新增 `status` 枚举（对齐上游 `DownloaderLoginResult.Status`）；
- 计数口径与上游 `AbstractDownloader.login()` 逐分支对齐：**只有 `INCORRECT_CREDENTIAL`
  与 `login0` 外抛异常（如 Transmission 无 try/catch）才计数**；qB / BitComet / Deluge /
  BiglyBT / Aria2 的 `login0` 内部 catch 异常返回 EXCEPTION / NETWORK_ERROR，**不计数**——
  此前 Rust 对 qB 的传输错误计数，会导致上游没有的「下载器短暂宕机恢复后仍被冷却
  最长 30 分钟（期间不拉取、不封禁）」盲区；
- 各适配器按上游 `login0` 语义标注状态（qB 会话校验自捕获 / API Key 与口令失败 →
  INCORRECT_CREDENTIAL；BitComet 版本不达标 → MISSING_COMPONENTS；BiglyBT 传输错误 →
  NETWORK_ERROR 等）；
- 冷却期内每次登录尝试发布 WARN 告警（identifier `downloader-too-many-failed-attempt-<id>`，
  push=true，按 identifier 去重），对齐上游 `publishAlert`；
- `login_gates` 改为 wave 与 Web 后端共享，下载器更新/删除时移除对应闸门（对齐上游
  `unregisterDownloader + registerDownloader` 使失败计数随实例重建清零——用户改对密码后
  立即恢复，不再受旧冷却影响）；
- `pbh` 主程序把数据目录暴露为 `PBH_DATA_DIR`：修复 `--data` 模式下 expression-engine
  脚本目录（`<data>/scripts`）解析不到的问题（此前只有 CWD 相对路径回退）。

> 注：上游 `ExpressionRule.maxScriptExecuteTime = 1500` 是死字段（声明后全文件无引用），
> 本移植保留 1500ms 兜底属**更严格**的防御性行为（方向安全：只会把超时脚本判为 pass）。

### perf/观测: 与实机部署 Java PBH 的 dry-run 并行对跑

直接在用户实机部署的 Java PBH（同配置、同下载器、同 GeoIP 库）旁边并行跑 Rust `--dry-run`
（只读接入，不向任何真实下载器下发封禁），实测一轮 10 分钟、5 个 wave 样本：

| 指标 | Rust | Java(实机 v9.5.1) | 结论 |
|---|---|---|---|
| 稳态 RSS | **102 MB** | 851 MB | 内存约为 Java 的 **1/8** |
| 单轮 wave 中位 | 2116 ms | 87 ms | **Rust 明显偏慢，待定位** |

- wave 耗时口径已核对可比：Java 的 `startTimer`（`DownloaderServerImpl.java:190`）设在
  ban wave 最开头，覆盖「计划任务 + 解封过期 + 登录 + 拉取 + 判定 + 下发」；
  Rust 的 `耗时` 为 `engine.run_once` 全程，两者一致。
- **慢因已定位并修复（隔离实验）**：差距全部来自环境里那个连不上的下载器
  `127.0.0.1:9093`。单一下载器对照（`check-interval` 临时调到 10s 取样）：

  | 只保留的下载器 | 单轮 wave 中位 |
  |---|---|
  | 在线 qB@9091 | 186 ms |
  | aria2@30000 | 1 ms |
  | **不可达 qB@9093** | **2003 ms**（六轮恒为 2002–2004） |
  | 全部三个 | 2101 ms |

  - 该 2s **不是**本项目的超时配置（`CONNECT_TIMEOUT_SECS=10`），也不是系统代理
    （曾尝试对下载器客户端 `.no_proxy()` 验证，实测无变化，已回退该改动）。
  - **裸 TCP 实测**：`127.0.0.1:9093` 连接耗时 **2016ms 后失败**，属操作系统层面
    （防火墙丢包导致等待 SYN 超时），任何程序连接该端口都要付这笔开销。
  - 真正的差异是**重试策略**：Java 五轮里只有一轮是 2631ms（≈2s，即那一轮才真正去连），
    其余 76–188ms —— 说明上游**不会每轮都去连持续失败的下载器**；
    而 Rust 每轮都重试登录，于是每轮固定付 ~2s。
  - ⇒ 已在 Rust 侧对齐上游的失败退避策略，见「fix(wave)」条目。
- 行为侧：该窗口内真实流量无可封禁 peer，两版封禁数均为 0（平凡一致）；
  判定等价性此前已由 mock 对跑证明（同 4 个 IP、同规则）。

### fix(wave): 连续登录失败的下载器进入冷却（对齐上游 AbstractDownloader）

上游 `AbstractDownloader.login()` 在 `failedLoginAttempts >= 15` 后把 `nextLoginTry`
推到 `now + 30min`，此后 `login()` **立即返回、完全不发网络请求**（并发布一条 WARN 告警）。
本移植此前缺这一层，于是每个不可达的下载器都会让每一轮 ban wave 白付一次连接超时。

- 新增 `wave::LoginGate`（挂在 `WaveEngine.login_gates`，按下载器 ID 存放）：
  `MAX_ATTEMPTS = 15`、`COOLDOWN_MS = 30min`；冷却期内 `login()` 直接返回失败。
- 计数口径：上游对 `IOException`/`Throwable` 递增，返回的 `LoginResult` 仅
  `INCORRECT_CREDENTIAL` 递增；本移植的 `LoginResult` 无状态码，故**只在 `Err`
  （网络/IO 异常）时递增**，避免把版本不兼容等非凭据原因也计入冷却。
- **实测验证**（只挂 `127.0.0.1:9093` 这个不可达下载器，`check-interval=10s`）：
  wave#1–15 每轮 2003–2060 ms，第 15 次触发冷却，wave#16–24 **全部 0 ms**。
- 单测：`wave::tests::login_gate_cools_down_after_max_attempts`（冷却边界与到期恢复）。

### 新增观测：wave 单轮耗时

- `crates/pbh/src/main.rs`：wave 日志增加 `耗时={}ms`（Java 侧日志自带 `(Nms)`，
  Rust 此前没有，无法与实机/对跑对等测量）。

### feat(script): AviatorScript 兼容层——社区 `.av` 脚本原样运行（补齐实机对跑暴露的忠实度缺口）

上游 `expression-engine` 与 BTN 脚本规则均为 **AviatorScript**；本移植此前要求用户把脚本
手写改写成 rhai，导致实机部署的 PBH-BTN 社区脚本（`gopeed-random-peerid.av`、
`name-id-verify.av`、`2e0-61ff-fe.av`、`dot-1-ipv6-tr296.av`）在 Rust 侧编译失败被跳过、
自定义规则不生效。现新增 `crates/pbh-core/src/avscript.rs` 兼容层：

- **翻译器 `transpile`**（词法级扫描，字符串/注释/嵌套括号感知）：`##` 注释、单引号字符串、
  `string.*` / `seq.*` / `isBlank` / `toLowerCase` / `toString` 等内建、语句级裸赋值、`nil`
  全部自动翻译为 rhai；无法翻译的构造（三目、`=~`、`string.split` 等）显式报错跳过（对齐上游
  「编译失败 → 跳过该脚本」）。
- **保序 map（正确性关键）**：`seq.map(...)` 翻译为扁平数组 + `av_keys`/`av_get` 保序访问函数，
  而非 rhai 的 `#{}` map——rhai 的 Map 按键排序（BTreeMap），而 Aviator 保持**插入序**；
  `name-id-verify.av` 依赖 `'aria2explorer'` 先于 `'aria2'` 被前缀匹配，否则
  `aria2explorer` 客户端会被误判为伪装封禁。
- **共享引擎 `build_script_env`**：expression-engine 与 BTN 脚本规则统一走同一构造；
  同时注册上游驼峰 + snake_case 两套 getter，并按上游 `Peer`/`Torrent` 接口补齐
  `peer.peerAddress.{ip,port,address}`（`address` 对齐 `IPAddressUtil.getIPAddress(ip)
  .toPrefixBlock().toString()`——无前缀单地址返回规范化地址串，社区脚本据此做
  `endsWith("::1")`/`contains(":2e0:61ff:fe")` 判定）、`torrent.completedSize`/`private`/
  `seeding`/`hashedIdentifier`。
- **语义实证**（部署 JRE + aviator 5.4.4 / ipaddress 5.6.2 jar 探针）：`isBlank`/`toLowerCase`/
  `toString`/`string.*`/`seq.*` 行为、`indexOf` 未命中返回 -1、整数除法截断均与 rhai 一致或已对齐；
  此前迁移文档「rhai `3/2 == 1.5`」的记载有误，已更正（两语言整数除法都截断）。
- **黄金测试**：4 个实机社区脚本原样固化为夹具（`tests/fixtures/scripts/`），
  `tests/av_script_golden.rs` 锁定每个脚本的 BAN/pass 判定与 **reason 原文**（含伪装检测的
  拼接消息、插入序敏感的 `aria2explorer` 用例、`peer.peerAddress.address` 的 IPv6 特征段用例）；
  另有 16 个翻译器/引擎单测。
- 全量 `cargo test --workspace` 523 个测试通过，clippy 零警告。

### 待办（实机对跑暴露的忠实度缺口）

（无——表达式脚本缺口已由本条关闭。）

### fix(remap): 封禁列表补齐 IPv4 的 IPv4-mapped IPv6 变体

由 L5 双跑（Java v9.5.1 与 Rust 连同一个 mock 下载器、同一份 fixture）发现的忠实度缺口：

- `remap::equivalent_forms` 此前把「IPv4 → IPv4-mapped IPv6」按空操作处理、不生成，
  而上游 `IPAddressUtil.generateRemappedPairIfPossible` 对 IPv4 恒附带 `::ffff:a.b.c.d`。
  缺少该变体时，同一主机改走 IPv6 栈连入不会被 qBittorrent 阻断（双跑中 Java 下发了
  `::ffff:c612:b` 等条目，Rust 侧缺失）。
- 现按上游分支顺序实现（IPv4 分支优先）：IPv4 → 映射写法；NAT64 / Teredo → 内嵌 IPv4。
  IPv4-mapped IPv6 输入仍先归一为 IPv4，再由映射配对补回（`parse_addr` 已处理）。
- **影响面**：全量封禁列表与 `/blocklist/p2p-plain-format` 对每个 IPv4 封禁地址
  会多输出一条 `::ffff:` 映射条目（与上游一致）。
- 同步更新 golden 断言：`crates/pbh-core/tests/remap_golden.rs`、
  `crates/pbh-golden/tests/l3_qb_parse.rs`，以及 aria2 / biglybt / deluge / bitcomet
  四个下载器适配器的封禁载荷测试。

## 已提交

### feat(mockqb): mock qBittorrent 服务（对跑 / 性能基准夹具）

- 新增 `crates/pbh-mockqb`：由 fixture JSON 提供 qB v2 API 子集（auth/login、app/version、
  app/buildInfo、app/preferences、app/setPreferences、torrents/info、torrents/properties、
  sync/maindata、sync/torrentPeers、transfer/banPeers），使 Java 与 Rust 两版能吃同一份
  确定性输入，用于 L5 双跑与性能基准。
- 支持 `--record`：把收到的封禁下发（增量 `banPeers` 与全量 `banned_IPs`）逐 IP 追加录制，
  两版封禁集合因此可直接 diff。
- `fixtures/sample.json`：1 torrent / 7 peer，覆盖 peer-id 黑名单、多拨（同 /24 三 IP >
  `tolerate-num-ipv4`）、进度作弊候选与两个对照组。
- 双跑环境要点（踩坑）：
  - 须关闭 `ip-database.auto-update`，否则启动会卡在从 GitHub 下载 20MB 的 mmdb（本机约 15KB/s）。
  - mock 端口需避开已被占用的 8080；Java 侧 WebUI 需改端口（9898 常有已初始化的实例，
    而 OOBE 路由只在未初始化时注册）。
  - Java 侧用 `-Dpbh.datadir=<dir>` 隔离数据目录，再经 `POST /api/oobe/init` 设置 token
    并注册指向 mock 的下载器。

### feat(rulesub): 规则订阅 Web 后端与 DB 落库

补齐 IP 规则订阅（`module.ip-address-blocker-rules`）的实质性缺口：

- 实现 `SubModule`（`crates/pbh/src/submodule.rs`），使 `/api/sub/*` 真正可用：
  - `GET /api/sub/` 订阅规则列表（含运行时条目数）
  - `PUT /api/sub/rule` 新增、`PATCH /api/sub/rule/{id}` 更新、`DELETE /api/sub/rule/{id}` 删除
    （增删改即时回写 `config.yml` 的 `module.ip-address-blocker-rules` 段并触发一次刷新）
  - `POST /api/sub/rules/update`、`POST /api/sub/rule/{id}/update` 手动刷新
  - `GET /api/sub/logs` 更新历史（分页）、`GET/PUT /api/sub/interval` 刷新间隔
- `rulesub::refresh_all` 现在写入 `rule_sub_log`（更新历史）与 `rule_sub_info`（当前状态），
  对齐上游 `RuleSub*Service` 落库行为；`AppState.sub_module` 由此前恒为 `None`（致 404）改为
  模块启用时挂载真实后端。
- 调度器：启动立即拉取一次，之后按 `check-interval` 周期刷新（对齐上游 `initialDelay=0`）。
- `pbh-db`：新增 `count_rule_sub_log` / `get_rule_sub_info` 公开方法，`list_rule_sub_log` 增加分页 `offset`。

### 测试

- `crates/pbh/src/rulesub.rs`：新增 `refresh_all_writes_rule_sub_log_and_info_on_cache_parse`、
  `refresh_all_skips_disabled_rule_in_db` 两个集成测试，覆盖缓存解析落库与禁用订阅只更新状态。
