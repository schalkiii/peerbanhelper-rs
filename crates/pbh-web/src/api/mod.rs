//! 对齐上游 WebAPI 控制器的完整路由组与共享小工具。
//!
//! 路由分两组：`public_routes()`（无需鉴权：登录、manifest）与
//! `api_routes()`（需要 Token：其余全部）。与上游 Role 的对应关系：
//! Role.ANYONE → public；Role.USER_READ / USER_WRITE → authed
//! （本实现不区分读写角色，Token 一致即可）。

pub mod alerts;
pub mod auth;
pub mod bans;
pub mod btn;
pub mod charts;
pub mod downloaders;
pub mod general;
pub mod logs;
pub mod manifest;
pub mod peers;
pub mod push;
pub mod statistics;
pub mod sub;
pub mod tasks;
pub mod torrents;

use axum::{
    routing::{delete, get, patch, post, put},
    Router,
};
use std::collections::HashMap;

/// 不需要 Token 鉴权的路由（对齐上游 Role.ANYONE 的端点）。
pub fn public_routes() -> Router<crate::AppState> {
    Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/metadata/manifest", get(manifest::manifest))
        .route("/init/token", get(manifest::init_status))
        .route("/oobe/status", get(manifest::init_status))
}

/// 需要鉴权的路由（对齐上游 Role.USER_READ / USER_WRITE 的全部端点）。
///
/// 与上游的差异：`RuleSubController` 在模块未启用时**不注册** `/api/sub/*` 路由
/// （404）；本实现恒注册，由 `SubModule == None` 时返回 404（状态码一致）。
pub fn api_routes() -> Router<crate::AppState> {
    let mut router = Router::new()
        // —— 封禁列表 / 日志 / 排行（PBHBanController）——
        .route("/bans", get(bans::list).put(bans::add).delete(bans::remove))
        .route("/bans/logs", get(bans::logs))
        .route("/bans/ranks", get(bans::ranks))
        // —— 告警（PBHAlertController）——
        .route("/alert/{id}/dismiss", patch(alerts::dismiss))
        .route("/alert/dismissAll", post(alerts::dismiss_all))
        .route("/alert/{id}", delete(alerts::delete_alert))
        // —— 日志（PBHLogsController：history + SSE live）——
        .route("/logs/history", get(logs::history))
        .route("/logs/live", get(logs::live))
        // —— 后台任务（PBHBackgroundTaskController：SSE live，Role.USER_READ）——
        .route("/tasks/live", get(tasks::live))
        // —— 全局通用（PBHGeneralController）——
        .route("/general/status", get(general::status))
        .route(
            "/general/global",
            get(general::global_get).patch(general::global_patch),
        )
        .route(
            "/general/config",
            get(general::config_get).put(general::config_put),
        )
        .route(
            "/general/profile",
            get(general::config_get).put(general::config_put),
        )
        .route("/general/reload", post(general::reload))
        .route(
            "/general/checkModuleAvailable",
            get(general::check_module_available),
        )
        .route("/general/heapdump", get(general::heapdump))
        .route("/general/stacktrace", get(general::stacktrace))
        // —— 统计（PBHMetricsController）——
        .route("/statistic/counter", get(statistics::counter))
        .route("/statistic/analysis/field", get(statistics::field))
        .route("/statistic/analysis/date", get(statistics::date))
        .route("/statistic/analysis/banTrends", get(statistics::ban_trends))
        .route("/statistic/rules", get(statistics::rules))
        // —— 图表（PBHChartController）——
        .route("/chart/geoIpInfo", get(charts::geo_ip))
        .route("/chart/trend", get(charts::trend))
        .route("/chart/traffic", get(charts::traffic))
        .route("/chart/sessionAnalyse", get(charts::session_analyse))
        // —— 下载器管理（PBHDownloaderController）——
        .route(
            "/downloaders",
            get(crate::downloaders).put(downloaders::create),
        )
        .route("/downloaders/test", post(downloaders::test))
        .route(
            "/downloaders/{id}",
            patch(downloaders::update).delete(downloaders::remove),
        )
        .route("/downloaders/{id}/status", get(downloaders::status))
        .route("/downloaders/{id}/torrents", get(downloaders::torrents))
        .route(
            "/downloaders/{id}/torrent/{torrent_id}/peers",
            get(downloaders::peers),
        )
        // —— peer 与种子信息（PBHPeerController / PBHTorrentController）——
        .route("/peer/{ip}", get(peers::info))
        .route("/peer/{ip}/accessHistory", get(peers::access_history))
        .route("/peer/{ip}/banHistory", get(peers::ban_history))
        .route("/peer/{ip}/btnQuery", get(peers::btn_query))
        .route("/torrent/query", get(torrents::query))
        .route("/torrent/{info_hash}", get(torrents::details))
        .route(
            "/torrent/{info_hash}/accessHistory",
            get(torrents::access_history),
        )
        .route(
            "/torrent/{info_hash}/banHistory",
            get(torrents::ban_history),
        )
        // —— 推送（PBHPushController）——
        .route("/push", get(push::list).put(push::create))
        .route("/push/test", post(push::test))
        .route("/push/{name}", patch(push::update).delete(push::remove))
        // —— 规则订阅（RuleSubController）——
        .route("/sub/", get(sub::list))
        .route("/sub/rules", get(sub::list))
        .route("/sub/rule", put(sub::add_rule))
        .route("/sub/rules/update", post(sub::refresh_all))
        .route(
            "/sub/rule/{id}",
            patch(sub::update_rule).delete(sub::remove_rule),
        )
        .route("/sub/rule/{id}/update", post(sub::refresh_rule))
        .route("/sub/logs", get(sub::logs))
        .route(
            "/sub/interval",
            get(sub::interval_get).patch(sub::interval_patch),
        )
        // —— BTN / AutoSTUN 模块状态 ——
        .route("/modules/btn", get(btn::status))
        .route(
            "/modules/auto-stun-port-forwarding",
            get(btn::auto_stun_status),
        );

    // 旧版 `/api/ban/list` 等路径保留兼容（v4 前端）
    router = router
        .route("/ban/list", get(crate::ban_list))
        .route("/ban/logs", get(crate::ban_logs))
        .route("/metrics/general", get(crate::general_metrics))
        .route("/modules/swarm-tracking", get(crate::swarm_tracking))
        .route(
            "/modules/swarm-tracking/details",
            get(crate::swarm_tracking_details),
        )
        .route("/alerts", get(crate::alerts));
    router
}

/// 分页参数（对齐上游 `Pageable` / `Paginable`：`page` + `pageSize`，兼容 `size`）。
///
/// 缺省 `page=1`、`pageSize=50`；`pageSize` 夹到 1..=500（上游 v9 不分页时无上限，
/// 这里沿用既有实现的上限约定，避免一次拉全表）。
pub fn pagination(params: &HashMap<String, String>) -> (i64, i64) {
    let page = params
        .get("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 500);
    (page, size)
}

/// 解析 `orderBy`（对齐上游 `Orderable` 的 `field|asc/desc`，可重复）。
pub fn parse_order_by_params(params: &HashMap<String, String>) -> Vec<(String, bool)> {
    params
        .iter()
        .filter(|(key, _)| key.as_str() == "orderBy" || key.as_str() == "sorter")
        .flat_map(|(_, value)| {
            let value = crate::percent_decode(value);
            let mut parts = value.split('|');
            let field = parts.next().unwrap_or_default().to_string();
            let asc = match parts.next() {
                None => true,
                Some(direction) => {
                    !(direction.eq_ignore_ascii_case("desc")
                        || direction.eq_ignore_ascii_case("descend"))
                }
            };
            vec![(field, asc)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `orderBy`/`sorter` 的值形如 `field|asc|desc`，按值解析（对齐上游 `Orderable`）。
    /// 此前 BTN 上报误用 `parse_order_by`（吃整段查询串），传值会恒返回空——此处锁定正确行为。
    #[test]
    fn parse_order_by_params_decodes_field_and_direction() {
        let mut p = HashMap::new();
        p.insert("orderBy".into(), "banAt|desc".into());
        assert_eq!(parse_order_by_params(&p), vec![("banAt".to_string(), false)]);

        // 缺省方向为 ASC
        let mut p = HashMap::new();
        p.insert("orderBy".into(), "banAt".into());
        assert_eq!(parse_order_by_params(&p), vec![("banAt".to_string(), true)]);

        // 显式 asc
        let mut p = HashMap::new();
        p.insert("orderBy".into(), "field|asc".into());
        assert_eq!(parse_order_by_params(&p), vec![("field".to_string(), true)]);

        // 大小写不敏感，且 `descend` 同样视为降序
        let mut p = HashMap::new();
        p.insert("sorter".into(), "ip|DESC".into());
        assert_eq!(parse_order_by_params(&p), vec![("ip".to_string(), false)]);
        let mut p = HashMap::new();
        p.insert("sorter".into(), "ip|descend".into());
        assert_eq!(parse_order_by_params(&p), vec![("ip".to_string(), false)]);
    }

    #[test]
    fn parse_order_by_params_keeps_order_and_ignores_others() {
        // 可重复出现，按出现顺序作为主次排序键
        let mut p = HashMap::new();
        p.insert("orderBy".into(), "banAt|desc".into());
        p.insert("sorter".into(), "ip|asc".into());
        p.insert("page".into(), "1".into());
        p.insert("size".into(), "20".into());
        assert_eq!(
            parse_order_by_params(&p),
            vec![("banAt".to_string(), false), ("ip".to_string(), true)]
        );

        // 无 orderBy/sorter 键 -> 空
        let mut p = HashMap::new();
        p.insert("page".into(), "1".into());
        assert!(parse_order_by_params(&p).is_empty());
    }
}
