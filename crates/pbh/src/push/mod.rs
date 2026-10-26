//! 推送（Push）与告警（Alert）子系统。
//!
//! 对齐上游（v9.5.1）：
//! - `util/push/PushManager` + `PushManagerImpl`：按配置注册渠道、逐个推送、单渠道失败隔离、
//!   整体成功 = 任一渠道成功（OR）；
//! - `util/push/PushProvider` + `AbstractPushProvider`：渠道契约与 markdown → HTML 渲染；
//! - `util/push/impl/*`：九个渠道（pushplus / serverchan / smtp / telegram / bark / pushdeer /
//!   gotify / ntfy / webhook）的请求构造与成功判据；
//! - `alert/AlertManager` + `AlertManagerImpl` + `AlertLevel`：告警发布、identifier 去重、
//!   标题前缀、日志级别映射。
//!
//! 与上游的差异（逐条在下文注明）：
//! 1. 配置节名为 `push:`（上游为 `push-notification:`），键 = 渠道名、段内 `type` 决定类型；
//! 2. 告警落库（`AlertService`/`alert` 表）未移植 ⇒ `identifierAlertExists*` 的去重状态改为
//!    进程内集合（重启即丢失，WebUI 也读不到历史告警）；`markAlertAsRead` 只影响该集合；
//! 3. `GuiManager.createNotification`（JavaFX 通知）以相同级别的 tracing 事件替代；
//! 4. 出站 HTTP 走 `pbh_downloader::http::HttpFetcher`（生产环境为 `ReqwestFetcher`，
//!    测试注入 mock）；
//! 5. 上游没有「按告警级别路由渠道」的逻辑：`AlertLevel` 只进入标题前缀
//!    `[PeerBanHelper/<LEVEL>] ` 并透传给渠道（`NtfyPushProvider.mapLevelToPriority` 是
//!    未被调用的死代码，见 [`AlertLevel::ntfy_priority`]）。

mod manager;
mod provider;
mod services;
mod smtp;

pub use manager::*;
pub use provider::*;
pub(crate) use services::*;
pub use smtp::*;

#[cfg(test)]
mod tests;
