# 架构概览

> 面向需要改代码的人：先看 crate 边界与数据流，再决定改动落在哪一层。
> 行为契约以 [SPEC.md](../SPEC.md) 为准；本文只描述「现在是怎么组织的」。
> 文中的 ban wave / live snapshot / PCB / 对跑 / 黄金测试等简称，与用户文档用词的对应关系见
> [README「词汇对照」](../README.md#词汇对照)。

## 1. 设计约束

- **行为对齐上游**：Java PeerBanHelper v9.5.3 是唯一行为标准；判定、配置字段、
  下载器 API 语义都要逐 peer 一致，差异由 `pbh-golden` 的黄金测试捕获。
- **前端原样复用**：WebUI 使用上游 `webui/dist`，Rust 只负责提供同形 API 与静态资源；
  前端问题在服务端修，不 fork 前端。
- **单二进制优先**：`pbh.exe` 内含规则引擎、Web 服务与内嵌文案；GUI 只是可选壳。

## 2. Crate 依赖

```
pbh (bin)  ──►  pbh-web (axum API + 静态托管)
   │                │
   │                ├──► pbh-db (SQLite)
   │                └──► pbh-downloader
   │                        │
   ├────────────────────────┴──► pbh-core (规则引擎 / 模型 / 配置 / 工具)
   │
   └──► pbh-golden (测试)      pbh-mockqb (qB 模拟服务，独立 bin)
```

`pbh-gui` 是**独立 workspace**（Tauri），不依赖上述 crate，只以子进程方式拉起 `pbh.exe`。

| crate | 职责 |
|---|---|
| `pbh-core` | 纯逻辑：模型、配置解析、规则模块、ban 流水线、PTR/GeoIP/remap、BTN 协议、AutoSTUN 核心、i18n |
| `pbh-downloader` | 下载器抽象与各实现（qBittorrent / Transmission / Deluge / BiglyBT / BitComet）、自动发现、`live` 快照 |
| `pbh-db` | SQLite schema 与 DAO；兼容 Java 版库的列类型差异（如 TEXT 型 `peer_progress`） |
| `pbh-web` | axum 路由、鉴权、SSE、静态资源回退；`webui_endpoints.json` 为前端契约清单 |
| `pbh` | 组装：读配置、构建模块与下载器、跑 wave 主循环、启动 web/BTN/规则订阅等后台任务 |
| `pbh-golden` | 黄金/属性测试：以 Java 行为为基准校验判定与序列化 |
| `pbh-mockqb` | 本地 qBittorrent WebUI 模拟器，供下载器集成测试与手工联调 |

## 3. 一次 ban wave 的数据流

1. **采集**：`pbh/src/wave.rs` 按 `check-interval` 触发，遍历在线下载器，
   拉取种子与 peer（`pbh-downloader` 的 live 快照，避免逐 peer 重查属性）。
2. **归一**：组装 `TorrentData` / `PeerData`，应用 remap（NAT 翻译、Teredo、NAT64）。
3. **判定**：`pbh-core/src/pipeline.rs` 顺序执行 `RuleModule`，按上游 `PeerAction`
   优先级聚合；bypass 地址直接跳过。
4. **处置**：对 `BAN` / `BAN_FOR_DISCONNECT`：
   - 写入内存 `BanList` 与 `history` 表（`BAN_FOR_DISCONNECT` 不落库）；
   - 下发下载器封禁；累计 metrics；更新 live 快照供 WebUI 读取；
   - 若开启 `lookup.dns-reverse-lookup`，把 PTR 回填任务投递到固定大小 worker 池，
     完成后异步写回 `banMetadata.reverseLookup`（不阻塞 wave）。
5. **收尾**：按上游周期持久化 `banlist`、清理过期封禁、广播 wave 心跳。

模块清单见 `crates/pbh-core/src/modules/mod.rs`（client-name、peer-id、progress-cheat、
anti-vampire、multi-dialing、idle-protection、ip/string/ptr 黑名单、BTN、表达式引擎等）。

## 4. 线程与异步模型

| 执行体 | 说明 |
|---|---|
| tokio 主 runtime | `#[tokio::main]`；wave 定时循环、web 服务、规则订阅刷新都跑在这里 |
| `spawn_blocking` | GeoIP 更新、Web 侧 PTR 查询等阻塞操作 |
| PTR worker 池 | `pbh-core/src/ptr.rs`：wave 回填 4 线程 + 系统解析器 4 线程，队列表线程数恒定 |
| BTN transport | 独立 OS 线程（长轮询/提交不占 tokio worker） |
| GUI 壳 | `pbh-gui` 进程监视并拉起 `pbh.exe`；附加模式下只开壳 |

SQLite 通过 `pbh-db::Database` 串行化访问；写路径（history/PCB/banlist）集中在 wave
与 Web 后端，避免跨线程持有连接。

## 5. 配置与兼容

- 支持上游**分文件布局**（`config/config.yml` + `profile.yml`）与单文件 `config.yml`。
- 保存配置时**保留未知键**：Java 版新增字段不会被 Rust 版覆盖丢失。
- 调试开关统一走 `pbh_core::external_switch`（环境变量 / 启动参数），清单见 SPEC §7。
- 数据库 schema 在 `crates/pbh-db/src/schema.sql`；迁移需保持与 Java 版同表同列。

## 6. Web 层

- `public_routes()`：登录、manifest、OOBE、彩蛋 `/api/egg`，无需 Token。
- `api_routes()`：其余 `/api/**`，经 `auth_middleware` 校验 Bearer / `?token=` /
  `PBH_SESSION` cookie；未初始化时 303 到 `/init`。
- 静态资源：`data/static`；SPA 子路由回退会注入 `<base href="/">`，并把
  `*/api/**` 308 到 `/api/**`（详见 AGENTS.md 的 SPA 门禁）。
- 前端契约：`crates/pbh-web/src/webui_endpoints.json` 固化「端点 + data 形状 +
  关键字段」，`webui_contract_all_endpoints_exist_and_match_shape` 全量校验。

## 7. 测试分层

| 层 | 位置 | 关注点 |
|---|---|---|
| 单元测试 | 各 crate `#[cfg(test)]` | 解析、换算、DTO 形状、错误分支 |
| 下载器集成 | `crates/pbh-downloader/src/*/mod.rs` 内 mock HTTP | 登录、限速、暂停、失败路径 |
| Web 契约 | `crates/pbh-web/src/lib.rs` + `webui_endpoints.json` | 端点存在、data 形状、嵌套字段 |
| 黄金/属性测试 | `crates/pbh-golden` | 与 Java 判定一致、PCB/remap 回归 |
| 在线联调 | `pbh-mockqb` | qB 会话/peer 抓取的真实 HTTP 形状 |

## 8. 改动落点速查

| 要改什么 | 改哪里 |
|---|---|
| 规则判定 / 新模块 | `pbh-core/src/modules/` + `pipeline.rs` + 配置解析 |
| 下载器 API | `pbh-downloader/src/<client>/`，先对照上游 Java 同名实现 |
| Web API 字段/端点 | `pbh-web/src/api/` + `api/mod.rs`，同步 `webui_endpoints.json` |
| 配置字段 | `pbh-core/src/config.rs`、`pbh/src/config.rs`、`default-config.yml` |
| 持久化 | `pbh-db/src/schema.sql` + DAO；保持 Java 库兼容 |
| 发布/部署 | `scripts/deploy.ps1`、`docs/DEPLOY.md`、CHANGELOG |
