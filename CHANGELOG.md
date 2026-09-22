# 变更日志（CHANGELOG）

本文件记录 peerbanhelper-rs 的功能新增、缺陷修复与行为变更，按时间倒序排列。
提交信息遵循 `type(scope): 中文描述` 约定。

## 未发布（working tree）

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
