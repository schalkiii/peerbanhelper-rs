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

## 目录与约定

- crates：`pbh`（bin + 组装）、`pbh-core`（规则引擎/模块）、`pbh-downloader`、`pbh-db`、
  `pbh-web`（axum）、`pbh-golden`（黄金测试）、`pbh-mockqb`；`pbh-gui` 独立不在 workspace
- 文档分工：README（现状）、SPEC.md（行为契约）、PLAN.md（进度/差异跟踪）、
  docs/TESTING.md（覆盖矩阵）、docs/DEPLOY.md（生产切换）、CHANGELOG.md（变更）
- 上游语义对照：改 Web API 前先读上游对应 Controller（`src/main/java/.../webapi/`）；
  前端是上游 dist 原样复用（`data/static/`），**前端问题在 Rust 服务端修**

## 当前状态（2026-10-08）

- 生产运行：GUI + pbh（health 200）、看门狗 wave 心跳常驻；WebUI 前端完整可用
  （会话 cookie / 缓存策略 / SPA 回退 / 下载器 CRUD id 语义均已修复）
- 演练模式（`server.dry-run`）支持 Web 配置页 + `PUT /api/general/dryrun` + GUI 托盘开关，
  均热生效
- pending-6（36h 长跑冻结）已降级观察项：修复后长跑与生产无真冻结复现；
  wave 全链路 phase 打点保留为下次冻结的定位手段

## 下一步候选

- plan 中未完成的扩展项（MySQL/PostgreSQL 支持等）见 PLAN.md
- 若再出现「冻结循环」，先核对看门狗阈值与实际 wave 间隔（教训见 docs/TESTING.md）
