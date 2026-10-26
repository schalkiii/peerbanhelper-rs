# 项目全面审计报告（2026-10-25）

> 审计对象：`D:\workspace\peerbanhelper-rs`（PeerBanHelper v9.5.1 的 Rust 移植）
> 对照基线：`D:\workspace\upstream-PeerBanHelper`（tag `v9.5.1`）
> 方法：XERJ 双索引（`pbh-rs` / `pbh-java`）检索 + 源码逐文件核对 + 路由/端点清单 diff +
> 全量测试与 clippy + 文档链接扫描。审计阶段只读，不修改业务代码。

## 1. 现状描述

| 项 | 数值 |
|---|---|
| Rust 源码 | 63,638 行 / 113 个 `.rs` 文件 |
| Java 基线 | 56,595 行（`src/main/java`） |
| workspace crates | 8 个（`pbh` / `pbh-core` / `pbh-downloader` / `pbh-db` / `pbh-web` / `pbh-golden` / `pbh-mockqb`；`pbh-gui` 独立） |
| 项目 Markdown | 13 篇（README / SPEC / PLAN / CHANGELOG / AGENTS / docs/*） |
| 测试 | 666 个用例 / 28 个测试目标全部通过；clippy 全 workspace 零警告 |
| 前端契约 | `webui_endpoints.json` 73 个前端实际调用端点，契约测试全部通过 |
| 当前发布 | `9.5.1-rs.4`（测试/文档增量）；生产运行 rs.3 构建（功能等价），health 200 |
| 构建产物 | `target/` 已清理，仅保留 `target/release/{pbh.exe,pbh-gui.exe,WebView2Loader.dll}` |

结构：工作区按「核心规则引擎 / 下载器 / 数据库 / Web / 二进制装配 / 黄金测试 / mock」分层，
`WebBackend` 与 `Downloader` trait 提供可注入边界；行为契约集中在 SPEC，进度与差异在 PLAN。

质量水平：规则模块、下载器适配器、封禁链路与 Web API 的**已使用面**与上游逐项对齐；
测试以黄金夹具与契约测试为主；文档与实现基本同步；主要遗留是**未使用的上游后端接口**、
**超大模块的可维护性**、**无 CI 与无行覆盖率**。

## 2. 七个维度结论

| 维度 | 结论 |
|---|---|
| 文档一致性 | 基本一致：测试数、发布状态、路由/字段说明均为现役；发现 1 处注释与实现不符（`autostun.rs` 仍称「未实现 AutoSTUN」，实际核心能力已实现，仅 Web 状态面为静态/501）。 |
| 文档缺漏 | 缺少架构文档与人类可读 API 索引；`docs/DEPLOY.md` 使用 `C:\PBH-RS` 示例路径，与生产实际路径不同（示例性质，非错误）。 |
| 架构优化点 | 模块边界清晰，但存在 5 个超大文件（最大 4,522 行）；PTR 反查按封禁/查询创建 OS 线程，缺少统一并发上限；GUI 独立 workspace 无测试。 |
| 代码写法 | clippy 零警告、无 TODO/FIXME；未发现 P0/P1 逻辑缺陷。潜在维护风险集中在超大文件与线程生命周期。 |
| 测试覆盖率 | 666 用例，关键规则/适配器/Web 契约均已覆盖；**Windows GNU 无法运行 llvm-cov（缺 profiler_builtins）**，无行覆盖率；`pbh-gui` / `pbh-mockqb` / `main` 覆盖较弱。 |
| 测试严谨性 | 黄金测试对关键载荷逐字段断言；但 `webui_endpoints.json` 契约测试只校验 `data` 形状，不校验嵌套字段名；曾多次发生前端静默显示 0/NaN 的字段错配，未来仍可能复发。 |
| 工程规范 | `Cargo.lock` 入库、版本/发布流程有文档；但**无 `.github/workflows` CI**、**无 `rust-toolchain.toml`**，构建工具链依赖本机约定；发布为手动脚本。 |

## 3. 问题清单

| ID | 位置 | 问题 | 严重度 | 影响范围 |
|---|---|---|---|---|
| A1 | `crates/pbh-web/src/api/autostun.rs:1-7` | 文档注释称「本移植未实现 AutoSTUN」，与 `PLAN.md §3.6`（TCP STUN/UDP 探测/转发器已实现，仅 Web 状态面静态）冲突 | 低 | 误导后续维护者，可能重复实现或误删核心能力 |
| A2 | `docs/DEPLOY.md` 步骤示例 | 示例路径 `C:\PBH-RS` 与生产 `C:\CommonTools\PeerBanHelper-rs` 不同；无部署产物校验步骤（sha256/health 之外） | 低 | 运维照抄示例时路径错位 |
| B1 | `webui_endpoints.json` + `webui_contract_all_endpoints_exist_and_match_shape` | 仅校验端点存在与 `data` 形状；嵌套字段改名不会被发现 | 中 | 前端静默显示空白/0/NaN；历史上同类问题复发 5 次 |
| B2 | 工程根目录 | 无 CI workflow；测试/clippy 仅靠本地执行 | 中 | 回归可能绕过门禁合入 |
| B3 | 工程根目录 | 无 `rust-toolchain.toml`，GNU 工具链依赖 AGENTS 约定与本机 PATH | 低 | 新环境/CI 可能选错工具链（MSVC linker 不可用） |
| C1 | `crates/pbh-core/src/btn_transport.rs`（4,522 行）、`modules/monitor.rs`（3,281）、`pbh-web/src/lib.rs`（2,656）、`pbh/src/push.rs`（2,206）、`bitcomet/mod.rs`（2,237） | 单文件职责过多（协议/状态机/HTTP/映射/测试混在一起） | 中 | 维护成本、review 难度、并行开发冲突 |
| C2 | `crates/pbh/src/wave.rs`（PTR 回填处）、`crates/pbh-core/src/ptr.rs` | 每封禁/每查询创建 OS 线程；DNS 慢时线程数无上限 | 中 | 极端情况下线程膨胀、内存/调度压力（PTR 默认关闭，风险有条件） |
| D1 | Web API 路由表 vs 上游 | 上游存在但 Rust 未实现的接口：`/api/sub/logs/{ruleId}`、`/api/peer/{ip}/btnQueryIframe`、`/api/autostun/tunnel/{id}/info`、`/api/general/refreshNatStatus`、`/api/config/*`、`/api/task/*`、`/api/utilities/replaceTracker`、`/api/statistics_list/get`、`/api/chart/{clientAnalyse,sessionBetween,sessionDayBucket}`、`/api/plugins*`、`/api/egg`、`/api/v2`、`/api_v2/*`、`/api/webui/login`、`/api/device_token/get` | 中/低 | 前端 contract 未调用；外部客户端/未来前端可能依赖。前 4 项为小接口，后 10 项为未使用/旧版/非目标 |
| D2 | `webui_endpoints.json` 与上游路由 diff | `/api/torrent` 上游已注释，Rust 只保留 `/api/torrent/query`，**不是缺口** | — | 已核对，无需修改 |
| E1 | 测试工具链 | Windows GNU 无 `profiler_builtins`，`cargo llvm-cov` 不可用 | 中 | 无法给出行覆盖率，只能用例/分支清单量化 |
| E2 | `pbh-gui` / `pbh-mockqb` / `main` | GUI 474 行无测试；mockqb 仅 3 个测试；main 仅 2 个 | 低 | GUI/装配层回归依赖人工冒烟 |
| E3 | `downloaders_scan_returns_empty_list` | 测试真实扫描本机 LISTEN 端口，耗时 8–14s 且依赖本机服务状态 | 低 | CI 变慢/潜在环境相关失败 |
| F1 | `crates/pbh-core/src/external_switch.rs` 测试 | 通过 `set_var` 修改进程环境变量；测试并行时存在全局状态竞争 | 低 | 偶发测试不稳定 |
| G1 | 发布流程 | `gh release upload` 实测多次 TLS reset，需要重试；本地无发布产物校验脚本 | 低 | 发布偶发失败/需人工重试 |

## 4. 改进建议

> 每项给出做法、文件清单、验证方式；优先级见 §5。

### P2-1 补齐 4 个小接口（建议本轮执行）

1. **`GET /api/sub/logs/{ruleId}`**
   - 做法：`SubModule::logs` 增加 `rule_id: Option<&str>`；`submodule.rs` 传给
     `db.count_rule_sub_log(rule_id)` / `list_rule_sub_log(rule_id, ...)`；新增路由与 handler。
   - 文件：`crates/pbh-web/src/lib.rs`（trait）、`crates/pbh/src/submodule.rs`、
     `crates/pbh-web/src/api/sub.rs`、`crates/pbh-web/src/api/mod.rs`、`crates/pbh-db/src/lib.rs`（已支持）。
   - 验证：新增 `/api/sub/logs/{id}` 分页测试，断言只返回该 rule 的日志；现有 `/api/sub/logs` 不变。
2. **`GET /api/peer/{ip}/btnQueryIframe`**
   - 做法：`BtnAbilitySpec` 增加 `iframe_endpoint`；`BtnAbility` 保存该字段；
     `BtnNetwork` 增加 `ip_query_iframe_url(ip)`（拼接 ip/appId/appSecret/hardwareId/installationId）；
     Web handler 返回 URL 字符串（无 BTN/无能力时复用现有错误码）。
   - 文件：`crates/pbh-core/src/btn_transport.rs`、`crates/pbh-web/src/api/peers.rs`、
     `crates/pbh-web/src/api/mod.rs`。
   - 验证：单测断言 URL 查询参数完整；无 iframe endpoint 时返回
     `BTN_ABILITY_IP_QUERY_NOT_PROVIDED`；不破坏现有 `btnQuery`。
3. **`GET /api/autostun/tunnel/{id}/info`**
   - 做法：按上游 `TunnelInfoDTO` 返回静态「未启用」结构（与现有 autostun 读端点语义一致）。
   - 文件：`crates/pbh-web/src/api/autostun.rs`、`api/mod.rs`。
   - 验证：契约形状测试 + 404/空数据不 panic。
4. **`POST /api/general/refreshNatStatus`**
   - 做法：复用 `autostun::refresh_nat_type`（当前返回 501；至少保证路由存在，不返回 `API_NOT_FOUND`）。
   - 文件：`crates/pbh-web/src/api/mod.rs`。
   - 验证：路由存在测试（501 或 200，非 `API_NOT_FOUND`）。

### P2-2 提升契约测试严谨性（建议下一轮）

- 做法：在 `webui_endpoints.json` 为高频端点增加 `required_fields`（如 bans 的
  `address/banMetadata/peer/ipGeoData`；downloaders 的 `id/name/type/endpoint`；
  statistics 的 `checkCounter/peersBlockRate`），测试逐字段断言。
- 文件：`crates/pbh-web/src/webui_endpoints.json`、`crates/pbh-web/src/lib.rs`（形状检查函数）。
- 验证：故意改掉一个字段名，测试必须失败。

### P2-3 建立 CI（建议下一轮）

- 做法：`.github/workflows/ci.yml`，Windows runner + GNU 工具链（安装 mingw/rustup target），
  执行 `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`；
  Linux runner 可选跑 `cargo llvm-cov` 产出覆盖率。
- 文件：`.github/workflows/ci.yml`、`rust-toolchain.toml`。
- 验证：提交后 workflow 通过；覆盖率 artifact 可下载。

### P2-4 拆分超大模块（建议按文件逐个进行）

- 做法（按优先级）：
  1. `btn_transport.rs` → `btn/{config,ability,pow,transport,submit}.rs`；
  2. `pbh-web/src/lib.rs` → `router.rs` / `static.rs` / `middleware.rs` / `tests/`；
  3. `modules/monitor.rs` → `monitor/{session,swarm,recording,cache}.rs`；
  4. `push.rs` → `push/{providers,manager,smtp,webhook}.rs`。
- 文件：上述文件及 `mod.rs` 声明。
- 验证：每次拆分只移动代码不改逻辑，`cargo test --workspace` + clippy 全绿；对比拆分前后测试数一致。

### P3-1 限制 PTR 线程（建议与 P2-4 同期）

- 做法：PTR 回填改为固定大小 worker 池（例如 2–4 线程）或有界信号量；
  `PtrLookup::lookup` 的系统解析线程仍保留超时，但由池调度。
- 文件：`crates/pbh-core/src/ptr.rs`、`crates/pbh/src/wave.rs`。
- 验证：新增“多个封禁不创建超过 N 个线程”的测试（可用计数注入）；PTR 结果仍能回填。

### P3-2 文档修正

- `autostun.rs` 顶部注释改为「核心能力已实现；Web 状态/写操作仍为静态/501」。
- `docs/DEPLOY.md` 明确生产路径与示例路径的区别，并补发布产物 sha256 校验步骤。
- 新增 `docs/ARCHITECTURE.md`（crate 依赖图、数据流、线程模型）与 `docs/API.md`
  （从 `webui_endpoints.json` 生成的人类可读索引）。
- 验证：Markdown 链接扫描 0 断链；文档描述能在源码找到对应实现。

### P3-3 测试稳定性

- `downloaders_scan_returns_empty_list` 改为注入可替换的 discovery，或标记 `#[ignore]` 并保留
  一条本地手动命令；`external_switch` 环境变量测试串行化（全局 `Mutex`）。
- 文件：`crates/pbh-web/src/lib.rs`、`crates/pbh-core/src/external_switch.rs`、`crates/pbh-downloader/src/discovery.rs`。
- 验证：`cargo test --workspace -- --test-threads=16` 多次运行稳定；耗时下降。

## 5. 优先级与并行性

| 优先级 | 项 | 并行性 |
|---|---|---|
| P1 | 无（已使用面与前端契约无缺失；无 P0/P1 缺陷） | — |
| P2 | P2-1 四个小接口 | 可并行：1/3/4 与 2 涉及文件部分重叠（`api/mod.rs` 需串行合并）；建议 2 单独、1/3/4 一组 |
| P2 | P2-2 契约字段校验 | 独立可并行 |
| P2 | P2-3 CI | 独立可并行 |
| P2 | P2-4 拆分模块 | 每个文件串行；不同文件可并行 |
| P3 | P3-1 线程池 | 依赖 P2-1 的 PTR 相关改动，需串行 |
| P3 | P3-2/P3-3 文档与测试稳定性 | 可并行 |

## 6. 已核对、明确不是缺口的项

- `/api/torrent`：上游代码中已注释，Rust 只实现 `/api/torrent/query`（前端契约使用后者）。
- `/api/modules/ipblacklist/*`：Rust 的泛型 `/api/modules/ipblacklist/{ruleType}` 覆盖
  `ip/port/asn/region/city/netType`，与上游等价。
- `/api/chart/sessionDayBucket`：当前前端使用 `/api/chart/sessionAnalyse`，不属于缺口。
- `/api/config/*`、`/api/task/*`、`/api/utilities/*`、`/api/plugins*`、`/api/v2*`：
  WebUI 未调用；属上游后端/旧版/插件能力，按 PLAN 的 non-goals 处理，不纳入本轮补齐。
  `/api/egg`（彩蛋）已在 2026-10-26 补齐，见 §9。
- `/api/peer/{ip}/btnQueryIframe` 之外的上游 peer 路由（`btnQuery`）已实现。
- 生产 health、发布资产 digest、XERJ 索引均已核对。

## 7. 审计结论

已使用面（前端契约 73 端点、规则/下载器/封禁链路）**与上游一致且无已知 P0/P1 缺陷**；
剩余差异集中在未使用的后端接口、超大模块可维护性、契约测试深度与工程化（CI/覆盖率）。
建议按 P2-1 → P2-2 → P2-3 的顺序小步补齐，P2-4/P3 作为后续维护项。

## 8. 执行记录（2026-10-25）

- 已关闭 D1 中的 3 项：`/api/sub/logs/{ruleId}`、`/api/autostun/tunnel/{id}/info`、
  `/api/general/refreshNatStatus`（实现与验证见 CHANGELOG `9.5.1-rs.5`）。
- 暂缓：`/api/peer/{ip}/btnQueryIframe`——上游 URL 需要 `hardwareId` 指纹，Rust 尚未实现
  等价指纹，避免伪造参数；`/api/config/*`、`/api/task/*`、插件/彩蛋/v2 等未使用或非目标
  接口继续保持不实现。
- 未实施建议：P2-2 契约字段校验、P2-3 CI、P2-4 模块拆分、P3-1 PTR 线程池，列入后续 backlog。

## 9. 执行记录（2026-10-26）

| 项 | 状态 | 说明 |
|---|---|---|
| `/api/egg` | 已完成 | 22 条链接照抄上游，无鉴权随机 302；见 CHANGELOG `9.5.1-rs.6` |
| P2-2 契约字段校验 | 已完成 | `fields`/`item_fields`/`results_fields` + 种子数据；空样本判失败 |
| P2-3 CI | 已完成 | `.github/workflows/ci.yml` + `rust-toolchain.toml`（GNU 三元组） |
| P3-1 PTR 线程池 | 已完成 | 回填池 4 线程 + 系统解析池 4 线程，附上界测试 |
| E3/F1 测试稳定性 | 已完成 | 扫描用例 skip/ignore；proxy 测试改注入 sink；web 套件 8.3s → 0.5s |
| 文档缺漏 | 已完成 | `docs/ARCHITECTURE.md`、`docs/API.md`、DEPLOY 校验指引、autostun/分页注释 |
| P2-4 模块拆分 | 已完成 | 五个超大文件全部拆分（2026-10-26，见 CHANGELOG `9.5.1-rs.7`）：`btn_transport` 按 DTO/PoW/network/util 拆分（最大 1,367 行）、`monitor` 按工具/数据/告警/四个服务拆分（最大 1,118 行）、`push` 按渠道/SMTP/管理器拆分（最大 780 行）、`bitcomet` 测试独立（生产 1,169 行）；`pbh-web/lib.rs` 测试迁至 `src/tests.rs` |
| `btnQueryIframe` | 维持暂缓 | 上游 `hardwareId = SHA256(oshi.getHardwareUUID())`（SMBIOS UUID，取不到时回退 MAC）；Rust 无等价 SMBIOS 读取，`MachineGuid` 不等价，不伪造参数。前端当前也未调用该端点 |
| E1 覆盖率 | 已登记限制 | GNU 工具链无 `profiler_builtins`，`cargo llvm-cov` 不可用；已在 docs/TESTING.md 说明，以用例/分支清单量化 |
| E2 GUI/mockqb/main 覆盖 | 维持 | 低优先级；GUI 仍以发布前人工冒烟为准 |
| G1 发布校验 | 已完成 | 新增 `scripts/verify-release.ps1`（sha256 + 包内文件齐全） |
