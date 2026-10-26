# AGENTS.md — PeerBanHelper-RS 协作约定

## 项目定位

PeerBanHelper（Java v9.5.1 基线，上游源码 `D:\workspace\upstream-PeerBanHelper`）的 Rust 忠实重写。
**行为对齐上游是硬约束**：封禁决策、配置字段、下载器 API 调用语义必须逐 peer 一致；
任何行为差异都是缺陷，由黄金测试捕获。生产部署目录：`C:\CommonTools\PeerBanHelper-rs`。

## 怎么跑起来

- 构建/测试/静态检查：`cargo build --release -p pbh` / `cargo test --workspace` /
  `cargo clippy --workspace --all-targets`
- **一键部署（改完代码必须用它，防「忘部署」）**：`pwsh -File scripts/deploy.ps1`
  （构建 pbh + GUI → 停服 → 复制到生产目录 → health 验证）
- 运行：`pbh --data ./data [--port 9898] [--dry-run]`；GUI：`crates/pbh-gui`（独立 workspace，
  需单独 `cd crates/pbh-gui && cargo build --release`）
- 工具链：**stable-x86_64-pc-windows-gnu**（rustup default；本机 MSVC BuildTools 已卸载）。
  MinGW 的 `cc` 是 C 构建依赖；**GNU coreutils 的 `link.exe` 会遮蔽 MSVC linker**，
  部署脚本已做 PATH 净化（仅移除 coreutils，保留 mingw）

## 参考实现检索（写码前必做）

改动规则判定、下载器语义、Web API 字段前，先检索上游 Java 如何实现同类逻辑，再对照本移植：

- XERJ 节点（本机常驻；未启动时 `xerj --insecure --port 9300 --data-dir ~/xerj-data`）：
  `xerj search --url http://127.0.0.1:9300 --prefix pbh-java "progress cheat excessive" -k 5`
- 已建两个索引：`pbh-rs`（本项目）与 `pbh-java`（上游 v9.5.1）；
  `xerj def --url http://127.0.0.1:9300 --prefix pbh-java <Symbol>` 可直接定位符号。
- 索引刷新：上游/本项目源码变动后重跑
  `xerj autoindex D:/workspace/upstream-PeerBanHelper --prefix pbh-java --url http://127.0.0.1:9300`
  （本项目同理，前缀 `pbh-rs`）。
- 检索结果只作线索，**以两侧源码为准**：输出里必须核对 Java 的 `file:line` 与
  Rust 对应实现，不能只凭检索片段下结论。

## 行为开关（调试/排障）

对齐上游 `ExternalSwitch`：环境变量可用原样键（`pbh.pcb.disable-completed-excessive=true`）
或大写下划线（`PBH_PCB_DISABLE_COMPLETED_EXCESSIVE=true`），启动参数 `--pbh.xxx=...` 优先。
已接线：PCB completed-excessive、idle ignore-if-any-interested、qBittorrent
bypassVersionCheck/enableRangeBanIp/ignoreRangeBanIpCondition/disableSameIpMultiConnection、
`pbh.downloader.<Name>.maxConcurrentPeerRequestSlots`、`pbh.forceDisableIPDB`、
`pbh.allowCors`。完整清单见 SPEC §7。

## 目录与约定

- crates：`pbh`（bin + 组装）、`pbh-core`（规则引擎/模块）、`pbh-downloader`、`pbh-db`、
  `pbh-web`（axum）、`pbh-golden`（黄金测试）、`pbh-mockqb`；`pbh-gui` 独立不在 workspace
- 文档分工：README（现状）、SPEC.md（行为契约）、PLAN.md（进度/差异跟踪）、
  docs/TESTING.md（覆盖矩阵）、docs/DEPLOY.md（生产切换）、CHANGELOG.md（变更）
- 上游语义对照：改 Web API 前先读上游对应 Controller（`src/main/java/.../webapi/`）；
  前端是上游 dist 原样复用（`data/static/`），**前端问题在 Rust 服务端修**
- **Web API 字段对齐门禁（同类缺陷已 5 次复发，改 `/api/**` 响应必做）**：
  字段名/结构对不上时前端**不报错**，而是静默显示 `0` / 空白 / `NaN undefined/s`——
  已三次出现（`peerBlockRate` 恒 0%、9a260e4 批量字段名、`rtUploadSpeed` 缺失致速度 NaN）。
  改动响应后必须逐字段核对两处真源：① 上游 Controller/DTO 实际输出名；
  ② dist 前端读取点（`data/static/assets/*.js`，压缩单行，用脚本抠 token 上下文，
  勿用 grep 上下文）。**字段名以「前端读什么」为准，不是以「上游构造参数叫什么」为准**
  （如 `UserIPTestResult` 字段是 `from/to/generatedCidr`，非构造参数 `lower/upper/compressed`）。
- **WebUI 端点契约门禁（改 `/api/**` 路由表或新增/删除端点必做）**：前端实际调用的全部端点与
  `data` 形状固化在 `crates/pbh-web/src/webui_endpoints.json`，测试
  `webui_contract_all_endpoints_exist_and_match_shape` 逐条校验「端点存在 + 形状匹配」。
  新增端点必须同步进清单，否则前端调用会静默拿到 `API_NOT_FOUND`；清单来源与刷新方式见
  docs/TESTING.md §6。**该机制上线即发现 9 个缺失端点 + 1 处形状不符**——手工挑事故点的
  回归测试覆盖不到「从未被请求过的端点」。
- **SPA 静态资源门禁（改 `static_handler` / 前端部署方式必读）**：dist 的 `index.html`
  用相对路径引用 `assets/api/favicon`，从非根子路由（如 `/list`）直接刷新时浏览器会
  请求 `/list/assets/...`、`/list/api/...`。服务端已做三层防护：① 回退 html 注入
  `<base href="/">`；② `*/api/...` 308 到 `/api/...`；③ 子路径 assets/favicon 映射。
  **动静态服务逻辑后必须用浏览器从子路由硬刷新验证**（curl 只能验 Content-Type，
  验不出前端把 HTML 当 JS/JSON 解析这类问题）。前端路由的真实路径见上游
  `webui/src/router/index.ts`（如封禁名单是 `/list` 不是 `/banlist`，设置是 `/config`
  不是 `/settings`）——**不要凭导航栏文案猜 URL**。

## 发布流程

1. CHANGELOG 归版（`## <版本>（YYYY-MM-DD）`），提交推送到 main
2. `git tag v<版本>` → `git push origin v<版本>`
3. `gh release create v<版本> <产物 zip> --title ... --notes-file ...`
   （产物：`pbh.exe` + `pbh-gui.exe` + `WebView2Loader.dll`，打包
   `pbh-rs-<版本>-windows-x64.zip`）
4. 版本号约定：Cargo 版本对齐上游基线（9.5.1），移植自身的发布序列以 `-rs.N`
   后缀体现在 tag / Release 标题上

## 当前状态（2026-10-25）

- **已发布 9.5.1-rs.4（2026-10-25；测试/文档增量，运行时与 rs.3 一致；上一版 rs.3）**，
  产物为 Windows x64 的 pbh.exe + pbh-gui.exe（+ WebView2Loader.dll）；tag/Release 与 main 同步
- **2026-10-25 全量 review + 能力补齐已推送 main 并部署生产**：
  Web API 契约修复、PCB 共享 range、PTR 反查（含 DoH/自定义端口）、ExternalSwitch、代理/CORS/EcoQoS、
  下载器自动发现、live snapshot、配置保存保留未知键；收尾另修复两项生产问题
  （`proxy.setting=0` 不再清空环境代理、BTN 兼容 Java 迁移库的 `peer_progress TEXT` 列）。
  667 测试全绿、clippy 零警告，生产 health 200 / wave#1 正常。
  该批运行时改动已发布 **9.5.1-rs.3**（tag `v9.5.1-rs.3`）；
  **9.5.1-rs.4** 为后续测试/文档增量发布（无生产代码变化，生产无需重新部署）。
- 生产运行：GUI + pbh（health 200）、看门狗 wave 心跳常驻；WebUI 前端可用
  （会话 cookie / 缓存策略 / SPA 回退 / 下载器 CRUD id 语义均已修复）
- 2026-10-09~13 连续修复一批「前端静默显示异常」缺陷（字段名/结构未对齐上游）：
  `peersBlockRate` 恒 0%、活动种子速度 `NaN undefined/s`、统计/图表/封禁日志端点字段对齐、
  `accessHistory`/`swarm-tracking details` 恒 500、SPA 子路由刷新资源错配（`/list` 刷新报
  「无法解析接口返回数据」）、`geoIpInfo` 返回对象致统计页 `.reduce is not a function`、
  规则订阅列表 `{rules,count}` 与字段名（`id/name/url` vs `ruleId/ruleName/subUrl`）、
  `/api/push` 包 `{channels}` 层）。根因与门禁见上文两条门禁条目，
  端点缺失类由 docs/TESTING.md §6 的契约清单门禁兜住
- 演练模式（`server.dry-run`）支持 Web 配置页 + `PUT /api/general/dryrun` + GUI 托盘开关，
  均热生效
- pending-6（36h 长跑冻结）已降级观察项：修复后长跑与生产无真冻结复现；
  wave 全链路 phase 打点保留为下次冻结的定位手段
- 工作区：`target/live/` 历史对跑归档（0.64 GB）与 `probe_java_db.py` 已于 2026-10-11
  经用户确认删除；`gen_bench_fixture.ps1`（无文档引用的早期基准脚本）与
  `longrun_pbh.log.err`（0 字节，被误提交入库）已于 2026-10-13 经用户确认删除，
  `.gitignore` 同步补 `*.log.err` 防复发

## 下一步候选

- plan 中未完成的扩展项（MySQL/PostgreSQL 支持等）见 PLAN.md
- 若再出现「冻结循环」，先核对看门狗阈值与实际 wave 间隔（教训见 docs/TESTING.md）
