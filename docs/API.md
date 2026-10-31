# Web API 索引

> 人类可读版；**机器可读的权威清单**是
> [`crates/pbh-web/src/webui_endpoints.json`](../crates/pbh-web/src/webui_endpoints.json)，
> 由测试 `webui_contract_all_endpoints_exist_and_match_shape` 逐条校验
> 「端点存在 + data 形状 + 关键字段」。改 `/api/**` 后必须同步该清单。

## 1. 通用约定

- **响应包**：除特别说明外均为 `{ "success": bool, "message": string|null, "data": any }`
  （上游 `StdResp`）。`data` 的形状按端点不同（数组 / 对象 / 数字）。
- **分页**：`page`（默认 1）+ `pageSize`（默认 10，上限 500，兼容 `size`）；
  `data = { page, size, total, results[] }`。
- **鉴权**（`/api/**` 除公开端点外）三选一：
  - `Authorization: Bearer <token>`
  - `Cookie: PBH_SESSION=<token>`（`POST /api/auth/login` 下发）
  - `?token=<token>`（兼容脚本/第三方）
  - token 未初始化时返回 303 → `/init`（`WEBAPI_NEED_INIT`）。
- **错误码**：未命中的 `/api/**` 返回 404 JSON `API_NOT_FOUND`（不会落回 SPA HTML）；
  明确未支持的功能返回 `success=false` + 语义化 message（如 `AUTOSTUN_NOT_SUPPORTED`）。

## 2. 公开端点（无需 Token）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/auth/login` | 登录，成功下发 `PBH_SESSION` cookie |
| POST | `/api/auth/logout` | 退出 |
| GET | `/api/metadata/manifest` | 版本、模块清单、installationId、遥测开关 |
| GET | `/api/init/token` | 是否已完成初始化 |
| GET | `/api/oobe/status` | 同上（OOBE 别名） |
| POST | `/api/oobe/testDownloader` | 向导：下载器连通性测试 |
| POST | `/api/oobe/testDatabaseConfig` | 向导：数据库配置测试 |
| POST | `/api/oobe/scanDownloader` | 向导：本机下载器自动发现 |
| POST | `/api/oobe/init` | 完成初始化（写 token/下载器等） |
| GET | `/api/egg` | 彩蛋：随机 302 到上游精选链接（无前端入口） |

## 3. 封禁与日志

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/PUT/DELETE | `/api/bans` | 封禁列表（分页；兼容 `limit+lastBanTime` 旧式拉取）/ 手动封禁 / 解封（`*` 全解） |
| GET | `/api/bans/logs` | 封禁历史（分页） |
| GET | `/api/bans/ranks` | 封禁 IP 排行（分页） |
| PATCH | `/api/alert/{id}/dismiss` | 忽略告警 |
| POST | `/api/alert/dismissAll` | 忽略全部告警 |
| DELETE | `/api/alert/{id}` | 删除告警 |
| GET | `/api/logs/history` | 历史日志（数组） |
| GET | `/api/logs/live` | 实时日志（SSE） |
| GET | `/api/tasks/live` | 后台任务进度（SSE） |

## 4. 通用 / 统计 / 图表

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/general/status` | JVM/系统/BTN/版本/运行时长（Rust 版以进程内存近似堆） |
| GET/PUT | `/api/general/dryrun` | 演练模式开关 |
| GET/PATCH | `/api/general/global` | 全局暂停等开关 |
| GET/PUT | `/api/general/config` | 完整配置读写（保留未知键） |
| GET/PUT | `/api/general/profile` | 同上（旧路径别名） |
| POST | `/api/general/reload` | 重载配置 |
| POST | `/api/general/refreshNatStatus` | 旧版 NAT 刷新入口（当前 501） |
| GET | `/api/general/checkModuleAvailable` | 模块类名/配置名是否可用 |
| GET | `/api/general/heapdump` | 堆快照（Rust 版为占位/诊断信息） |
| GET | `/api/general/stacktrace` | 线程栈（诊断信息） |
| GET | `/api/statistic/counter` | 检查/封禁/解封计数、封禁率、跟踪 swarm 数 |
| GET | `/api/statistic/analysis/field` | 按字段聚合（count/sum + filter） |
| GET | `/api/statistic/analysis/date` | 按日期聚合 |
| GET | `/api/statistic/analysis/banTrends` | 封禁趋势 |
| GET | `/api/statistic/rules` | 各模块命中统计 |
| GET | `/api/chart/geoIpInfo` | 按城市/ISP/省/地区聚合（四维数组） |
| GET | `/api/chart/trend` | 连接/封禁趋势 |
| GET | `/api/chart/traffic` | 流量曲线 |
| GET | `/api/chart/sessionAnalyse` | 会话分析 |

## 5. 下载器 / Peer / 种子

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/downloaders` | 下载器摘要数组（id/name/type/online/version/endpoint/paused） |
| PUT/POST | `/api/downloaders` | 新增下载器 |
| PATCH/PUT/DELETE | `/api/downloaders/{id}` | 修改 / 删除 |
| POST | `/api/downloaders/test` | 连通性测试 |
| POST | `/api/downloaders/scan` | 本机自动发现（裸数组） |
| GET | `/api/downloaders/{id}/status` | 实时状态（限速、暂停等）；`config` 为 **camelCase Web DTO**（见 docs/TESTING.md §7） |
| GET | `/api/downloaders/{id}/torrents` | 种子列表 |
| GET | `/api/downloaders/{id}/torrent/{torrentId}/peers` | peer 列表（含 PTR/GeoIP） |
| GET | `/api/peer/{ip}` | peer 详情（在线 GeoIP、PTR、BTN 可用性） |
| GET | `/api/peer/{ip}/accessHistory` | 访问历史（分页） |
| GET | `/api/peer/{ip}/banHistory` | 封禁历史（分页） |
| GET | `/api/peer/{ip}/btnQuery` | BTN IP 查询 |
| GET | `/api/torrent/query` | 种子检索（分页） |
| GET | `/api/torrent/{infoHash}` | 种子详情 |
| GET | `/api/torrent/{infoHash}/accessHistory` | 访问历史（分页） |
| GET | `/api/torrent/{infoHash}/banHistory` | 封禁历史（分页） |

## 6. 推送 / 规则订阅 / 脚本 / 实验

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/PUT | `/api/push` | 推送渠道列表 / 新增 |
| POST | `/api/push/test` | 测试推送 |
| PATCH/DELETE | `/api/push/{name}` | 修改 / 删除 |
| GET | `/api/sub/`、`/api/sub/rules` | 订阅规则列表（`data` 为裸数组） |
| PUT | `/api/sub/rule` | 新增订阅 |
| PATCH/DELETE | `/api/sub/rule/{id}` | 修改 / 删除 |
| POST | `/api/sub/rule/{id}/update` | 刷新单个订阅 |
| POST | `/api/sub/rules/update` | 刷新全部 |
| GET | `/api/sub/logs` | 刷新日志（分页） |
| GET | `/api/sub/logs/{ruleId}` | 单个订阅的刷新日志 |
| GET/PATCH | `/api/sub/interval` | 刷新间隔（`data` 为裸数字，毫秒） |
| GET | `/api/expression-engine/scripts` | 脚本列表 |
| GET | `/api/expression-engine/editable` | 内置可编辑脚本 |
| GET/PUT/DELETE | `/api/expression-engine/{id}` | 读取 / 保存（body 为源码文本；**语法不合法返回 400 `RULE_ENGINE_BAD_EXPRESSION` 且不落盘**） / 删除 |
| GET | `/api/laboratory/experiments` | 实验列表 |
| PUT | `/api/laboratory/experiment/{id}` | 开关实验 |
| POST | `/api/laboratory/config` | 实验配置 |

## 7. 模块状态 / AutoSTUN / 其他

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/modules/btn` | BTN 模块状态 |
| GET | `/api/modules/auto-stun-port-forwarding` | AutoSTUN 模块状态 |
| GET | `/api/modules/swarm-tracking` | 已跟踪 swarm 数量（裸数字） |
| GET | `/api/modules/swarm-tracking/details` | swarm 明细（分页） |
| GET/PUT/DELETE | `/api/modules/ipblacklist/{ruleType}` | IP 黑名单规则读写 |
| POST | `/api/modules/ipblacklist/ip/test` | 单 IP 规则测试 |
| GET/PUT/DELETE | `/api/pbhplus/status`、`/api/pbhplus/key` | PBH Plus 授权状态/密钥 |
| GET | `/api/pbhplus/claimRenewFreeLicenseCaptcha` | 免费续期验证码 |
| POST | `/api/pbhplus/renewFreeLicense` | 免费续期 |
| GET | `/api/autostun/status` | AutoSTUN 状态（当前恒「未启用」） |
| PUT | `/api/autostun/config` | 保存配置（当前 501） |
| GET | `/api/autostun/tunnels` | 隧道列表（空数组） |
| GET | `/api/autostun/tunnel/{id}/info` | 单隧道信息（`success=false`） |
| GET | `/api/autostun/tunnel/{id}/connections` | 隧道连接（空分页） |
| POST | `/api/autostun/refreshNatType` | 刷新 NAT 类型（当前 501） |
| POST | `/api/autostun/restart` | 重启 AutoSTUN（当前 501） |

> AutoSTUN 的**核心**（NAT 翻译 / TCP 隧道 / NAT 类型探测）已在
> `pbh-core::auto_stun` 中实现并受 `ip-remapping.auto-stun.enabled` 门控；
> 上表这组 **WebUI 管理端点尚未接线**，因此返回诚实的「未启用」状态。

## 8. 非 `/api` 端点

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 健康检查（运维/看门狗/部署脚本） |
| GET | `/blocklist/p2p-plain-format` | 纯文本封禁清单（下载器匿名拉取） |
| GET | `/blocklist/ip` | IP 清单 |
| GET | `/blocklist/dat-emule` | eMule dat 格式 |
| GET | `/*` | SPA 静态资源与前端路由回退 |

## 9. 兼容别名（旧版前端）

`/api/ban/list`、`/api/ban/logs`、`/api/metrics/general`、`/api/alerts` 保留用于
v4 前端；新前端请使用第 3、4 节的路径。
