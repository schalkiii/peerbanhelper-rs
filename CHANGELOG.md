# 变更日志（CHANGELOG）

本文件记录 peerbanhelper-rs 的功能新增、缺陷修复与行为变更，按时间倒序排列。
提交信息遵循 `type(scope): 中文描述` 约定。

## 未发布（working tree）

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
- **慢因已定位（隔离实验，未修复）**：差距全部来自环境里那个连不上的下载器
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
  - ⇒ 需要在 Rust 侧对齐上游的「失败下载器跳过/退避」策略，方能消除该差距。
- 行为侧：该窗口内真实流量无可封禁 peer，两版封禁数均为 0（平凡一致）；
  判定等价性此前已由 mock 对跑证明（同 4 个 IP、同规则）。

### 新增观测：wave 单轮耗时

- `crates/pbh/src/main.rs`：wave 日志增加 `耗时={}ms`（Java 侧日志自带 `(Nms)`，
  Rust 此前没有，无法与实机/对跑对等测量）。

### 待办（实机对跑暴露的忠实度缺口）

- **表达式脚本不兼容**：用户实机的 `expression-engine` 脚本（`gopeed-random-peerid.av`、
  `name-id-verify.av`）在 Rust 的 rhai 下编译失败被跳过（上游为 AviatorScript 语法），
  导致这些自定义规则在 Rust 侧不生效。

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
