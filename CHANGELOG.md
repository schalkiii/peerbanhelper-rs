# 变更日志（CHANGELOG）

本文件记录 peerbanhelper-rs 的功能新增、缺陷修复与行为变更，按时间倒序排列。
提交信息遵循 `type(scope): 中文描述` 约定。

## 未发布（working tree）

### feat(gui): 托盘「重启服务」菜单 + 外部链接转交系统浏览器

- **托盘右键新增「重启服务」**：结束 pbh 子进程并由监督线程立即拉起（约 2 秒，
  配置/数据无损）。实现为「kill + 置位立即重启标志」，**保持单 spawn 点**——
  避免托盘与监督线程竞态造成双子进程/端口冲突。
- **外部链接不再劫持主窗口**：主窗口迁移到代码创建（`WebviewWindowBuilder` +
  `on_navigation` 导航守卫）——本机 WebUI 与占位页正常加载，**其它链接一律转交
  系统默认浏览器打开**；修复「在 WebUI 点击外链跳走后无法返回」（此前外链会把
  整个 WebView 导航走且无返回手段）。
- README 托盘功能清单同步更新。



### fix(web): BTN 状态卡片真实根因——缺 `webapi-btn` FeatureModule 记录

- **上一轮修复不彻底**：只做了模块名大小写兼容（`BTN`→`btn`），但用浏览器实测
  渲染发现卡片仍显示「未启用」。
- **真实根因**（前端 fetch hook 抓到实际请求）：WebUI 的「BTN 状态」卡片查询的是
  `checkModuleAvailable?module=webapi-btn`——上游把 BTN 的上报/WebAPI 实现为独立
  FeatureModule `PBHBtnController`（`getConfigName()="webapi-btn"`），与规则模块
  `BtnNetworkOnline`（`"btn"`）是两个条目；Rust 的 `modules()` 只列规则流水线模块，
  缺 `webapi-btn` → 查询恒 false。
- **修复**：`modules()` 补充 `webapi-btn` 记录（对齐上游 FeatureModule 体系）。
- **验证方式升级**：本次以 agent-browser 驱动真实浏览器走完「登录 → 设置 → 状态
  标签」，截图确认卡片渲染为「模块状态：已启用 / 配置文件获取成功」——不再是
  仅验证 API 返回值。



### feat(gui/web): 演练模式一键开关（GUI 托盘 + dryrun API）

- **新增 `GET/PUT /api/general/dryrun`**：读写演练模式状态，PUT 落盘
  `server.dry-run` 并同步运行时标志（与配置页写回同路径，热生效无需重启）
- **GUI 托盘新增「演练模式（不向下载器下发封禁）」复选菜单**：点击切换调本机
  API（凭据复用静默登录 token）；启动就绪后自动同步后端当前状态
- 上游 WebUI 前端无此字段的图形开关（上游也无 dry-run 功能）——本项为 Rust 侧增强
- 实测：PUT true → 落盘 + GET 回读一致；workspace 测试 0 失败



### fix(web): general/status 补齐内存/IP 字段，修复 BTN 状态误显示未启用

- **BTN 状态卡片误显示「未启用」**：前端 `checkModuleAvailable?module=BTN` 用
  上游大写模块名查询，Rust 配置段名为小写 `btn` → 比较失败返回 `data:false`。
  模块名匹配改为大小写不敏感。
- **浏览器 IP 为空**：status 接口的 `system.network.client_ip` 此前硬编码空串；
  现按上游语义取 `X-Forwarded-For`/`X-Real-IP`（反代场景），否则取连接地址
  （serve 层加 `into_make_service_with_connect_info`，IPv4 映射地址折叠）。
- **内存总量/内存压力/堆内存信息**：
  - 补齐 `system.memory: {total, free, page_size}`（上游
    `generateSystemMemoryData` 同名字段，此前整段缺失）；
  - `jvm.memory.heap` 从「系统内存粗略拷贝」改为**进程真实工作集**
    （`GetProcessMemoryInfo.WorkingSetSize`），`max` 为系统物理内存总量——
    「堆内存信息」不再显示整个机器内存被占用的误导值。



### fix(web): 下载器 CRUD 对齐上游 id 语义，修复「添加下载器报 DL_NOT_FOUND / 405」

- **现象**：WebUI 添加 Aria2Next 下载器（JSON-RPC）报 `DL_NOT_FOUND`；实测
  `POST /api/downloaders` 直接 405。
- **根因**（三处叠加）：
  1. 路由方法缺失：`create` 只挂在 PUT（前端发 POST → 405）、`update` 只挂
     PATCH（前端发 PUT → 405）；
  2. id 匹配不一致：上游下载器 id 为 UUID（config 条目可显式配置 `id`），
     前端按 UUID 提交更新，而 `update_downloader`/`remove_downloader` 只按
     `name` 匹配 config → UUID 必然 `DL_NOT_FOUND`，删除时还会残留 config 条目；
  3. 新添加的下载器在下一轮 wave 前不出现在 list（statuses 驱动）。
- **修复**：路由补齐 `POST /downloaders` 与 `PUT/PATCH /downloaders/{id}`；
  update/remove/add-查重统一为 `name == id || config.id == id` 双匹配。
- **实测**：POST 添加 Aria2Next（`http://localhost:29100/jsonrpc`）→ success；
  wave 后 statuses `online=true`（JSON-RPC `getVersion`/`tellActive` 均通）；
  按 UUID PATCH 更新 200；workspace 测试 0 失败。



### feat(gui/web): 原版图标 + GUI 默认路径修复 + 启动占位页 + dry-run 可配置

- **图标**：`pbh-gui` 换用上游原版图标（`icon.png` 256px + 多尺寸 `icon.ico`
  16-256px），托盘/窗口/exe 资源同步更新。
- **GUI 默认路径**（修复「双击 pbh-gui.exe 不拉起 pbh.exe」）：`--pbh-path`/
  `--data-dir` 缺省值改为 exe 同目录布局（部署形态），开发环境回退
  `target/release` 布局；此前无参数启动指向 repo 相对路径，部署机上 spawn 必然失败。
- **启动 UX**：主窗口改为 `about:blank` 起始（不预加载服务地址），就绪检测
  30s→120s；超时显示本地「正在启动…」占位页并后台重试，就绪后自动切入
  WebUI——杜绝「找不到网页」错误页。
- **dry-run 可配置**：新增 `server.dry-run`（YAML kebab 对齐 `external-address`），
  与 CLI `--dry-run` 任一开启即生效；`WaveEngine.dry_run` 改为共享
  `Arc<AtomicBool>`，WebUI 配置页修改保存后热生效（`save_config`/`reload` 均同步），
  无需重启。WebUI 其余配置项（下载器/推送/规则订阅等）同样经 config 编辑 +
  热重建路径生效；wave 间隔等启动期参数需重启。
- **修复配置持久化 bug**：`PbhBackend::save_config` 此前固定写
  `<data>/config.yml`，而读取按上游布局优先 `<data>/config/config.yml`——写读
  路径不一致导致 WebUI 保存的配置重启即丢；backend 现持有 `config_path`
  （`load_or_create` 返回值），写回与读取同路径。
- **deploy.ps1**：纳入 GUI 构建与部署（此前只部署 pbh.exe）。



### feat(web): OOBE 端点与下载器扫描 + 契约测试补全（web 层全覆盖收官）

- **OOBE 向导端点**（Role.ANYONE，向导在认证前运行）：
  `POST /api/oobe/testDownloader`（复用 `backend.test_downloader`，返回
  success 布尔）、`POST /api/oobe/testDatabaseConfig`（内置 SQLite 恒成功）、
  `POST /api/oobe/scanDownloader`（mDNS 扫描未移植，返回空列表——手动添加
  为主路径）。此前这些端点缺失且被 SPA fallback 兜成假 200。
- **`POST /api/downloaders/scan`**：同上，返回空列表。
- **契约测试 +4**：push CRUD/test 全链路（PUT/GET/PATCH/test/DELETE）、
  oobe 三端点（无凭据直测——Role.ANYONE 语义）、downloaders/scan 空列表。
  **pbh-web 契约测试达 28 条、总用例 38 个**。
- **NoopBackend 语义统一**：全部写操作改 Ok（「空实现但成功」），测试聚焦
  响应结构而非后端状态。
- **看门狗自启**：启动文件夹注册 `pbh-watchdog.cmd`（登录自动拉起看门狗，
  免管理员权限；任务计划程序 ONLOGON 需提权故弃用）。
- 教训重申：release 构建遗漏再次发生——部署脚本化提上日程（见遗留）。
- 部署实测：oobe/scan 三端点 200；全量测试 0 失败。

### test(web): WebUI 契约测试集——把每个生产缺陷变成回归用例

### test(core/mockqb): 覆盖矩阵最后缺口消除——BTN L5 对跑实现、ptr L5 定性 n/a

- **BTN L5 对跑（矩阵 ❌ → ✅）**：mockqb 新增 BTN mock——`/btn/config` 返回
  协议 20/20 的 ability 清单（heartbeat/submit_bans/ip_denylist，endpoint 回指
  自身、random_initial_delay=1000——0 会使 Java `nextLong(0)` 崩溃）、上报事件
  录制进对跑 diff（`BTN:<事件>` 行，Sort-Unique 去重后自然比对）。注入脚本
  `inject_btn` 把两侧 `btn.config-url` 指向 mock 并启用模块。实测对跑：
  CONFIG_REQUESTED / heartbeat 上报（20B 逐字）/ IP_DENYLIST_SYNCED 三类事件
  全部「共有」——两侧 BTN 客户端行为完全一致。
- **ptr_blacklist L5 定性 n/a（非缺口）**：上游 `PeerBanHelper.java` 中
  `moduleClasses.add(PTRBlacklist.class)` 被注释——Java 生产不存在该判定，
  对跑无从比对。Rust 默认 `enabled: false` 与上游行为一致；L1/L2 已覆盖
  规则匹配与缓存。矩阵图例补充注⁴。
- 教训沉淀：nested group 的 re.sub 替换串不得重复拼接 group（group1 已含
  group2/3，重复拼接产生 `enabled:   enabled: true` 的 YAML 损坏）。

### test(web): WebUI 契约测试集——把每个生产缺陷变成回归用例

### test(web): WebUI 契约测试集——把每个生产缺陷变成回归用例

- **背景**：单元/黄金测试全绿但实机部署后前端大面积不可用——缺陷集中在
  「API 表面契约」（端点缺失/占位、响应结构与前端期望不符、认证流程断裂、
  `/api` 未命中被 SPA fallback 兜成假 200），属于既有测试的盲区：单测验证
  实现内部逻辑，不验证 API 表面与前端/上游的契约。
- **新增 9 条契约测试**（`lib.rs` tests mod，每条对应一个已发生的生产缺陷）：
  `btn_status_contract`（enabled/abilities/configSuccess 字段）、
  `general_status_contract`（btn 段/堆内存 available/compile_time/network）、
  `pbhplus_status_contract`（enabledFeatures 含 basic+paid）、
  `downloaders_list_contract`（endpoint/paused）、`downloader_status_contract`
  （lastStatus/activeTorrents/activePeers/config/paused）、
  `login_sets_session_cookie_and_authorizes`（login→Set-Cookie→cookie 过认证）、
  `silent_login_flow_sets_cookie_via_document`（文档入口 302+Set-Cookie 闭环）、
  `spa_fallback_and_api_404`（SPA 回退 html + /api 未命中 404）、
  `counter_reads_accumulated_metrics`（counter 读累计字段）。
- **配套修复**：`/api` 未命中端点改为 404 JSON（`api_not_found`）——此前冒泡
  到 SPA fallback 返回 index.html（假 200），掩盖后端缺端点。
- **教训**：改完源码后 `cargo build --release` 必须重跑——test profile 的
  验证不会更新 release 产物（本轮部署遗漏即此原因）。

### fix(web): general/status 占位字段填充 + pbhplus 端点（设置页运行状态修复）

### fix(web): general/status 占位字段填充 + pbhplus 端点（设置页运行状态修复）

- **设置页「网络/运行时信息/BTN 状态」大量占位**：`jvm.memory` 为空对象
  （首页显示「0 Bytes 可用」红点）、`compile_time: 0`（显示 1970-01-01）、
  `internet_access` 硬编码 false、`nat_type: unknown`、**无 `btn` 段**
  （设置页 BTN 状态读这里，显示「未启用」）。
- **修复**：
  - `jvm.memory.heap/non_heap`：系统物理内存（windows-sys 的
    `GlobalMemoryStatusEx`，`max/committed/used/available/init`；无 JVM 堆
    概念，用系统内存近似驱动「X 可用」显示）
  - `compile_time`：exe 修改时间戳（否则 1970）
  - `internet_access`：真实 TCP 探针（国内 baidu.com:443 / 国际
    cloudflare.com:443，900ms 超时，对齐上游内外网语义）；补
    `use_proxy/reverse_proxy/client_ip` 字段
  - 新增 `btn` 段：与 `/api/modules/btn` 同源（提取共享函数
    `btn::btn_status_data`）
- **Plus 订阅**：新增 `/api/pbhplus/status|key`（对齐上游 `PBHPlusController`）。
  上游为付费许可体系（`enabledFeatures` 由 license 聚合，前端以
  `enabledFeatures.includes('basic'/'paid')` 门控页面可用性）；Rust 版按用户
  要求**默认开启全部功能**：`enabledFeatures` 恒返回 `["basic", "paid"]`，
  `licenses` 为空。
- 实测：`heap.available≈1.94GB`；`internet_access={国内 true, 国际 false}`；
  `btn.enabled=True`；`compile_time=2026-10-07`；`enabledFeatures=[basic, paid]`。

### fix(web): 静默登录闭环修正——文档入口种会话 cookie（GUI 骨架屏根治）

### fix(web): 静默登录闭环修正——文档入口种会话 cookie（GUI 骨架屏根治）

- **headless Chrome 自主复现用户视角**：全新 profile 打开 `/?silentLogin=` 仍为
  骨架屏（下载器 0 处、skeleton 1 处）——上一版静默登录未闭环。
- **根因**：`?silentLogin=` 的豁免挂在 `/api` 的 auth_middleware 上，而**页面
  文档请求（GET /）不经过该中间件**——cookie 永远不会被种下；前端 API 请求
  又不携带 silentLogin（只有首页 URL 带），所以每次 API 仍 401。此前 curl 验证
  成功是因为手动在 API URL 上补了 silentLogin（真实前端不会）。
- **闭环修正**：static_handler（文档入口）检测 `?silentLogin=<token>` → 校验
  secret → 302 重定向到无参首页并下发 `PBH_SESSION` cookie → 页面内所有 API
  请求由浏览器自动携带 cookie 认证。`/api` middleware 的静默豁免保留（兼容）。
- **验证方法升级**：`chrome --headless --virtual-time-budget=12000 --dump-dom`
  （virtual-time 让异步 API 渲染完成后再抓 DOM——不带时 API 未返回，骨架屏
  假象会误导判断）。实测闭环后：**qBittorrent/MotrixNext 卡片真实渲染、
  skeleton=0**；正确 cookie 值调 `/api/metrics/general` → 200。

### fix(web): BTN 状态端点实现——设置页「BTN 模块未启用」假象

### fix(web): BTN 状态端点实现——设置页「BTN 模块未启用」假象

- `GET /api/modules/btn` 此前是硬编码占位（`"BTN is not available in this
  build"`），BTN 实际运行（ability 同步/上报日志正常）时设置页仍显示未启用。
- 对齐上游 `PBHBtnController.status` 的 `DownloaderStatusDTO` 语义：
  `enabled`（SharedBtnNetwork 是否有实例）、`configSuccess`/`configResult`
  （BtnConfigStatus 枚举转状态文本）、`abilities`（name/displayName/
  lastSuccess/lastUpdateAt）、`appId`/`appSecret`（>5 字符截断打码）、`configUrl`；
  未启用分支返回 `BtnNetwork == null` 的「需重启」结构（StdResp success=false）。
- `BtnNetwork` 补充 `config()` 配置快照访问方法（appId/appSecret/configUrl
  在 `BtnNetworkConfig`，Web 层原本无法触达）。
- GUI 静默登录导航增加诊断日志（写入 `data/pbh-gui.log`），失败不再静默。
- 实测：`enabled=True, configSuccess=True, abilities=8, configResult=SUCCESS`。

### feat(gui/web): 静默登录 + IP 规则黑名单管理端点（设置页功能修复）

### feat(gui/web): 静默登录 + IP 规则黑名单管理端点（设置页功能修复）

- **GUI 骨架屏（认证缺失）**：GUI 的 WebView2 有独立 cookie 存储，无人登录过 →
  全部 API 401。对齐上游 GUI 免登机制：`SILENT_LOGIN_TOKEN_FOR_GUI`（启动期随机
  UUID，Java 经 URL `&silentLogin=` 传给 WebUI，accessManager 豁免）——
  Rust 版 pbh 生成/持久化 `data/silent_login_token`；`auth_middleware` 对
  `?silentLogin=` 匹配的请求放行并下发会话 cookie；GUI 窗口就绪后导航到
  带 `?silentLogin=` 的 URL。前端零感知（cookie 自动携带）。
- **设置页 IP 规则黑名单管理缺失**：上游 `IPBlackList` 模块自注册
  `/api/modules/ipblacklist/{ruleType}` CRUD（GET/PUT/DELETE + ip/test），
  前端设置页的 IP/端口/ASN/地区/城市/网络类型规则管理全部依赖——Rust 缺失
  且被 SPA fallback 兜成 index.html（`/api/module/config` 类似，均为假 200）。
  新增 `api/ipblacklist.rs`：读写 `profile.yml` 的 `module.ip-address-blocker`
  段（经 `WebBackend::read/write_config`，兼容 `-`/`_` 键 normalize）；
  `netType` 按上游 Set 语义与 profile 的 8 键布尔对象互转；`ip/test` 返回
  CIDR 范围（对齐 `UserIPTestResult`）。
- **前端调用方式实测**（编译 JS 逆向）：所有请求带 `Authorization: Bearer
  ${authToken}`（登录后的内存凭据，无持久化）；PUT body 为 `{<ruleType>: <值>}`。
- 实测：silentLogin 无凭据调 metrics → 200 + Set-Cookie；ipblacklist
  GET/PUT/DELETE/city（含「浙江省 温州市」真实数据）全链路通过。

### fix(wave): counter 累计字段无人累加——WebUI「共检查/封禁/解封」恒为 0

### fix(wave): counter 累计字段无人累加——WebUI「共检查/封禁/解封」恒为 0

- `/api/statistic/counter` 读取 `Metrics.checks/peer_bans/peer_unbans`（累计字段），
  但 wave 只更新了「当前值」类字段（downloader_count 等），累计字段从未被写入，
  WebUI 首页「共检查 X 次 / 封禁 Peer X 次 / 解封 X 次」恒为 0。
- 修复：run_once 收尾处按本轮 report 累加（`checks += peers`、`peer_bans += banned`、
  `peer_unbans += unbanned`，对齐上游 BasicMetrics 的累计语义）。
  实测：首轮 wave 后 `checkCounter` 0 → 9。

### fix(gui): WebView2 磁盘缓存残留旧占位页的处置说明

- 生产教训的最后一环：部署早期缓存的占位页存在于 WebView2 用户数据目录
  （`%LOCALAPPDATA%\com.pbh-rs.gui\EBWebView`），服务端 `no-store` 只能阻止
  新缓存、无法清除已有缓存。处置：停止 GUI 后删除该目录（副作用仅为需要
  重新登录，凭据本身在服务端会话 cookie 中）。

### fix(web): SPA 子路由回退 + 下载器 list/status 对齐上游 DTO（WebUI 三缺陷）

### fix(web): SPA 子路由回退 + 下载器 list/status 对齐上游 DTO（WebUI 三缺陷）

- **GUI/子路由刷新显示占位页**：static_handler 对未命中路径一律返回
  「静态资源未安装」占位页——WebUI 是 history 路由 SPA，前端子路由
  （封禁名单/统计等）在磁盘无同名文件，刷新必中占位页；对齐上游 Jetty 的
  SPA fallback 语义：未命中回退 index.html（占位页仅用于前端未安装场景）。
- **下载器卡片状态「未知」**：上游 `DownloaderStatusDTO(lastStatus 枚举,
  lastStatusMessage, activeTorrents, activePeers, config, paused)`，前端状态
  文案读 `lastStatus`（HEALTHY/PAUSED/ERROR/…）；Rust 原 status 响应只有
  `{id, name, type, online, version}`，无该字段 → 显示 UNKNOWN。
- **编辑下载器弹窗全空**：前端从 status 响应的 `config`（上游
  `saveDownloaderJson()`）填充表单；Rust 缺失 → 弹窗空白。
- **修复**：`DownloaderStatus` 补 `torrents/peers` 计数（wave 聚合后回填）；
  `WebBackend` 新增 `downloader_meta(id)`（endpoint/配置快照，serde 序列化
  `DownloaderConfig` 保证与 PATCH 回写无损往返）；list 响应补 `endpoint/paused`
  （对齐 `DownloaderWrapperDTO`）；status 响应按 DTO 逐字段对齐
  （实测：`lastStatus=HEALTHY`、`activeTorrents=9`、`config` 完整）。

### fix(web): 登录下发会话 cookie，修复 WebUI 空白（数据全 0/骨架屏）

### fix(web): 登录下发会话 cookie，修复 WebUI 空白（数据全 0/骨架屏）

- **现象**：WebUI 页面外壳正常渲染但所有 API 数据为空——前端登录后全部请求 401。
- **根因**：上游为 Javalin 服务端会话（`sessionAttribute("authenticated", token)` +
  JSESSIONID cookie 由浏览器自动携带），且 WebUI 前端**没有任何手动凭据存储**
  （无 localStorage/请求拦截器，登录后完全依赖 cookie）；Rust 版 `auth/login`
  只校验返回 200、不下发任何 cookie，`auth_middleware` 也只认 Bearer/`?token=`，
  登录成功后后续 API 依然全部 401。
- **修复**：login 成功下发 `PBH_SESSION=<token>`（HttpOnly/Path=/，值即凭据，
  与 Bearer 同权；token 为 hex 无需编码）；middleware 增加会话 cookie 验证通道；
  logout 清除 cookie（对齐 `sessionAttribute(null)`）。
- **实测**：登录 → Set-Cookie → 带 cookie 调 `/api/metrics/general` 返回真实数据。

### docs(test): 新增测试覆盖矩阵与用例清单文档

- 新增 `docs/TESTING.md`：分层模型（L1 单元 / L2 模块与解析黄金 / L3 下载器集成 /
  L4 端到端 / L5 mock 对跑）、23 个测试目标 587 个用例的模块 × 覆盖矩阵、
  8 个 mock 对跑 fixture 的场景与断言清单、4 项已知缺口（pending）与
  对跑沉淀的 4 条上游行为语义。
- 本次覆盖率梳理的实测产出：`anti_vampire` / `pcb_desync` / `auto_range_ban` /
  `modules_matrix` 四个新对跑场景全部两侧一致；pending-1（PCB 过量下载累计在
  BanForDisconnect 后翻倍）经补充取证解除——Java 同样封禁（全量重放下发，
  BanForDisconnect 静默不打日志），1.8G 为 fastPcbTest 断开重连后重计的共同上游语义。
- 新增 `idle_protection.json` 对跑场景与 idle 加速参数注入（速度阈值 1e9、
  空闲上限 3s）；Java OOBE 会还原模块开关导致对跑不稳定，与注入时序一并
  列为 pending-2/3。
- 新增「生产部署替代 Java 版评估」（TESTING.md §5）：核心链路具备替代条件，
  前端复用上游 `webui/dist`、迁移即复制 data 目录；缺自更新器与数月级长周期数据，
  建议灰度并行迁移。

### test(mockqb): 多波 fixture 支持 + 确定性双跑基建参数化（行为类保真验证）

- **背景**：真实流量双跑存在架构性观测干扰（Java 先封禁 → qB 断开 peer → Rust
  永远看不到），只能验证长期稳定性，不能验证逐条判定保真。保真验证改用
  mockqb 确定性对跑：两侧连同一 mock 服务（peer 恒定在线、不真封禁），逐条比对。
- **mockqb 多波序列**：fixture 新增可选 `waves: [{torrents?, peers?}, ...]`，
  按 `torrentPeers` 请求次数轮转（超界恒用最后一份，未给出的沿用当前值），
  覆盖 PCB 进度回退 / 多拨累计等**跨波状态**场景；无 `waves` 时行为不变。
- **双跑脚本参数化**：`java_dualrun.ps1` / `rust_dualrun.ps1` 支持
  `-Fixture` / `-Tag`（录制文件按场景区分）；两侧复用 Java 现役 GeoIP 库
  （缺失时 Rust 启动会被在线补下阻塞）；diff 增加 IP 归一化（增量 raw_ip
  带端口 vs 全量不带端口，语义等价）。
- **三场景实测结论**：
  - 静态基线（sample）：判定集合完全一致（多拨 3 + PeerId 1）；暴露全量下发
    mapped 变体格式差异 → fix(remap) 修复。
  - PCB 进度回退（pcb_rewind，跨波 0.90→0.45）：两侧同波封禁、同模块
    （rewindProgress）、同格式，**跨波状态判定一致**。
  - 真实数据重放（Java history 3097 封禁 peer 回放）：**99.3%（954/961）一致**；
    仅 Rust 103 个全部为多拨子网收敛路径差异（上游 ForkJoinPool 并发竞态 +
    有限波数，上游注释明示「其他 IP 会在下一周期被封禁」，Rust 串行逐波补封
    即设计意图）；仅 Java 7 个中 4 个为 Java history 表自身的 IPv6 截断脏数据
    （尾冒号），其余为 dualrun 场景 BTN 输入不对齐。
- **工具**：`crates/pbh-mockqb/gen_replay_fixture.py`（从 Java 快照合成重放
  fixture；输出含真实 IP，默认落 gitignore 区不入库）。
- **回归用例扩展**（`eac0623`）：
  - `modules_matrix.json`：IP CIDR/单 IP/端口/城市（GeoCN 双温州 IP）/
    client-name REGEX/PeerId/握手豁免对照/不命中对照，10 个 peer 一次覆盖
    6 类判定路径；配 `inject_test_profile.py` 向两侧配置注入同一规则集
    （幂等、跟随序列既有缩进，避免 snakeyaml 因缩进不一致拒载整个 profile）。
  - `pcb_desync.json`：deSync 开窗跨波状态机（差值 20% 开窗 → 30s 窗口过期封禁）。
  - `auto_range_ban.json`：wave0 单 IP 封禁 → wave1 同 /30 peer AutoRangeBan 连锁。
  - `java/rust_dualrun.ps1`：修复 Java 慢启动下 profile.yml 未生成时注入静默
    失败的时序竞争（等待生成 + 失败即中止）。
  - 五个场景实测全部两侧封禁集合逐字一致；握手豁免（up/dl 全 0 跳过判定）
    与 REGEX 整段 matches() 语义两侧一致，均属上游行为。

### fix(web): `parse_order_by_params` 输出顺序确定化（pull 自带测试暴露）

- `HashMap` 迭代序不定导致 `orderBy` 与 `sorter` 并存时输出顺序随机（新增测试
  `parse_order_by_params_keeps_order_and_ignores_others` 间歇性失败）。
- 对齐上游 `Orderable`（LinkedHashMap 保序、仅认 `orderBy`）：改为固定键优先级
  先 `orderBy` 后 `sorter`（`sorter` 为移植版兼容别名，上游无此键）；补注释说明
  `HashMap` 查询层下重复键只剩单值的已知限制。

### fix(geoip): 内嵌行政区划表，修复 GeoCN 城市规则漏封（长跑对账发现）

- **问题**：12 小时双跑对账（`compare_dualrun`）发现 Java 侧 7 条封禁 Rust 全部漏掉，
  其中「IP 规则: 浙江省 温州市」类（`IPBlackList` 城市维度，对账期间 234 条）Rust 恒不命中。
- **根因**：`GeoCN2` 解析 rev2 记录依赖 `ok_data_level3.csv` 行政区划表——上游打包在 jar
  资源里永远可用；移植版只在 ipdb 目录查找，部署环境缺失该文件时 `DivisionTable` 为
  `None`，rev2 记录整条丢弃，GeoIP 查询结果 `city.name = null`，城市规则无法 contains 匹配。
- **修复**：将 `ok_data_level3.csv`（上游 jar 资源，GPL-3.0）随二进制内嵌
  （`include_str!`，对齐上游「jar 内资源永远可用」语义）；ipdb 目录同名文件仍可覆盖
  （便于更新区划数据），缺失/损坏时回退内嵌表。
- **回归用例**：`embedded_division_table_resolves_city_rule_prefix`（内嵌表按
  `330300000000` 逐级命中「浙江省 温州市」，join 格式与上游封禁 Reason 一致）。

### test(黄金对照): 补充细粒度用例与覆盖未覆盖模块

- **`ip-address-blocker-rules`（`ip_rule_list`）**：新增 `tests/l2_ip_rule_list.rs`，
  黄金对照解析（DAT/注释累积/尾注释/`level>=128` 丢弃/非对齐区间向下对齐到前缀块）、
  最长前缀匹配、`rule_key` 取订阅名、`reason` 走 `MODULE_IBL_MATCH_IP_RULE`、更新决策
  `plan_rule_update` 对齐上游 sha256 比对流程、握手中 peer 跳过。
- **`string_blacklist`**：细粒度用例补充 REGEX（整段 `matches()` 锚定）模式、
  `LENGTH`（UTF-16 码元区间含边界）模式，以及 `peer_id`/`client_name` 模块
  `data.type` 字段区分（`peerId` vs `clientName`）、`data.rule` 为规则串。
- **`pbh-web` `parse_order_by_params`**：新增单元测试锁定 `field|asc|desc` 解析
  （此前 BTN 上报误用 `parse_order_by` 导致排序参数恒空）。

### fix(移植对齐): 全面 review 修复与上游行为不一致

- **Web 鉴权**：`token` 为空时不再放行任何鉴权 API，改为 `303 /init`（对齐上游
  `WEBAPI_NEED_INIT`）；首次启动随机生成访问令牌并写回 `config.yml`（替代原匿名裸奔）。
- **统计接口**（`pbh-db`）：`ban_trends` / `field_stats` / `weeklySessions` 数据源改为
  `history` 表（JOIN `torrents`），对齐上游 `HistoryServiceImpl` / `PBHMetricsController`；
  按天分桶使用**本地时区当日 0 点**闭区间；`field_stats` 走上游 `mapField` 字段映射
  （`peerId` 按 `substringLength=8` 截断）。
- **种子查询**：修正关键词参数占位符绑定（`?1||'%'` 复用占位符导致 `SQLITE_RANGE`）；
  封禁数取自 `history.torrent_id`（原 `ban_logs` 为废弃表）。
- **下载器适配**：qBittorrent 的版本/登录/种子列表/peers/封禁下发均校验 HTTP 状态码，
  失败不再静默；Transmission 的 Basic Auth 在用户名或密码任一非空即带上（反代常见
  `空用户名+密码`），`blocklist-update` 失败仅记日志不抛出（对齐上游不阻断本轮）；
  HTTP 方法解析失败报错、响应体读取失败上抛（不再吞为 `GET`/空串）。
- **规则/模型**：`PeerFlag` 的 `is_from_incoming` / `outgoing_connection` 改为恒 `false`
  （`parseLibTorrent` 不设这两位，原按 `local_connection` 反推会把 NAT 误配告警条件写反）；
  `MultiDialingBlocker` 的 `keep-hunting-time` 默认值改为 `2592000s→2_592_000_000ms`；
  `StringBlacklist` 命中的 `data.rule` 改为命中规则的 `metadata()`（规则串，如 `-hp`），
  原为 peer 自身值；`peer_id` 截断按 **UTF-16 码元**口径（含 emoji 等增补平面字符不多吃字符）；
  i18n 反序列化新增 `null` 参数分支（上游 `null` 渲染为 `"null"`）；SMTP 正文补
  `text/html; charset=utf-8` 避免中文乱码。
- **空闲连接保护**：`idle-connection-dos-protection`（默认关闭）的 `onPeersRetrieved`
  在 `wave::run_downloader` 显式派发，缺失会让跟踪表只增不减；并修复除零。
- **BTN 上报**：`orderBy` 改用 `parse_order_by_params`（原 `parse_order_by` 传值恒空）；
  `DELETE /api/bans` 返回实际解封条数（上游 `count`）；订阅日志分页 1-based（off-by-one 修复）；
  `/blocklist/ip` 每行补 `/32` `/128` 前缀（下游按 CIDR 解析会丢弃裸地址）；
  历史上报循环加防死循环保护（游标不推进即退出）；遗留封禁上报游标初值取构造时刻
  （对齐上游 `lastReport = OffsetDateTime.now()`，避免首轮重报全部历史）；`reconfigure`
  后按 `kind` 重新定位 ability（避免下标串位）。

### feat(wave): 单条封禁日志（Lang.BAN_PEER）对齐上游

- 新增逐条封禁 INFO 日志（对齐 `DownloaderServerImpl` 第 252 行：**仅 `action != BAN_FOR_DISCONNECT`
  时打印**，ban-for-disconnect 静默）；
- 首参数复刻上游 `PeerAddress` 的 Lombok `@Data` `toString()` 全字段 dump
  （字段顺序一致；NAT/Teredo 翻译字段在默认直通场景恒为 `null`/`0`/`false`，与上游逐字一致）；
- 浮点参数按 **Java `Double.toString`** 语义格式化（整数值补 `.0`；`< 1e-3` / `>= 1e7`
  走 `1.0E-4` 风格科学计数法）——`Progress=` 与上游逐字可比；
- 由此长时对跑两侧的 `[封禁]` 行可直接 diff；新增 `wave::tests::ban_peer_log_helpers_match_upstream_formatting`。

### fix(wave): ban wave 完成日志对齐上游文案与计数口径（ProcessingStatistics）

- **计数口径**：`downloaders` / `torrents` 只统计「至少有一个 peer 通过判定」的条目
  （对齐 `DigestionSession.convertBanDetails` 的 `handled` 映射）——登录成功但 0 peer 的
  下载器、0 peer 的种子都不计入（实机对照：Java 与 Rust 同报「1 个下载器」，aria2 无 peer 被排除）；
- **日志文案**改用上游 i18n `BAN_WAVE_CHECK_COMPLETED`（含参数顺序），长时对跑可直接把
  两侧日志逐行 diff；附加诊断（在线下载器 / 跳过 / 错误数）降级到 DEBUG；
- 新增 `logger.hide-finish-log` 配置支持（上游语义：`true` 隐藏；下载器列表为空时不打印）；
- 新增测试 `wave_report_counts_follow_upstream_processing_statistics`。

### feat(dualrun): 长时对跑基建补齐（快照/水位/身份标记 + 对账增强）

- `longrun_sample.ps1`：新增**共享读 DB 快照**（主库 + `-wal` + `-shm`，`FileShare.ReadWrite`
  可在两侧运行中安全取证；周期 `-SnapshotEveryRounds` + 退出时最终快照）、**磁盘水位告警**
  （剩余空间 / 单库体积阈值，JSONL 打 `disk`/`db_warn` 字段）、退出时结束 Rust 子进程；
- `pbh` 新增 `--tag <str>` 长时对跑身份标记：写入 `metadata.dualrun_tag` 与
  `metadata.dualrun_started_at`，并随启动日志输出；
- `compare_dualrun` 增强：**IP 文本规范化**（两侧 IPv6 十六进制/点分映射写法差异不误报）、
  **命中模块集合比对**（同址同小时但模块无交集 = 真实行为差异，独立报告 + CSV `module-mismatch` 行）；
- 实机 Java 数据库经取证确认为 **SQLite**（`data/persist/peerbanhelper-nt.db`，魔数
  `SQLite format 3`），运行中可共享读快照——PLAN「Java 侧 H2 导出」待补项关闭；
- `prepare_live.ps1`：预建 `persist/` 使 Rust 采用上游 DB 布局（与快照/对账工具链路径一致）。

### feat(gui,dualrun): Tauri 原生 GUI 壳（M1 托盘）+ 长时对跑基建（采样脚本 + DB 对账工具）

- 新增 `crates/pbh-gui`（**独立 crate，不在 workspace members**）：tauri v2 托盘壳——子进程拉起
  `pbh`（崩溃 5 秒自动重启）、系统托盘（显示/浏览器打开/退出）、WebView 指向 `http://127.0.0.1:<port>`
  复用上游 WebUI、关窗=隐藏到托盘、附加模式（服务已在运行则不重复拉起）；
  Windows 侧 `cargo check` 通过（构建要求与里程碑见 PLAN「原生 GUI」）；
- 新增 `longrun_sample.ps1`：天级对跑健康采样（两侧 RSS/CPU/DB 大小/health → JSONL，Rust 崩溃自动拉起）；
- `pbh-db` 新增 `compare_dualrun` bin：长跑结束后的离线对账（`history` 按 IP+端口+小时桶匹配、
  列名容错探测、差异输出 CSV；在线逐条比对决策记录见 PLAN「长时对跑基建」）。

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
