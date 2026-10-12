//! 实验性功能端点（`/api/laboratory/*`，对齐上游 `util.lab.Laboratory`）。
//!
//! WebUI `service/labs.ts` 的契约：
//! - `GET  /experiments` → `CommonResponse<{labEnabled, experiments: Experiment[]}>`；
//! - `PUT  /experiment/{id}` → body `{status}`（`CommonResponseWithoutData`）；
//! - `POST /config` → body `{enabled}`（总开关）。
//!
//! 缺端点会让「启用实验性功能」落到 `/api` 的 JSON 404（界面报 API_NOT_FOUND）。
//!
//! 状态持久化在 `<config_dir>/laboratory.yml`：**总开关 `enabled`** + **各实验项
//! `true`/`false`/`"default"`**。`"default"` 时是否激活由**实验组**决定——
//! 组号 = `abs(installationId.hashCode() % 5)`（`Java String.hashCode()`），
//! 与上游 `Laboratory.experimentalGroup` 对齐，保证同样的灰度行为。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use pbh_core::i18n::TranslationComponent;
use serde_json::{json, Value};
use std::path::Path as StdPath;

use crate::AppState;

/// 实验注册项（对齐上游 `util.lab.Experiments` 枚举）。
struct ExperimentDef {
    id: &'static str,
    /// 参与的实验组（0..4）
    groups: &'static [i32],
    title_key: &'static str,
    desc_key: &'static str,
}

/// `async_banlist_apply` / `async_ipdb_loading` 的分组在 Java 枚举里尚未登记，
/// 按「全量灰度」（0..4）处理——文案表里已有对应 key，界面需要看到这两项。
const EXPERIMENTS: &[ExperimentDef] = &[
    ExperimentDef {
        id: "sqlite_vacuum",
        groups: &[0, 1, 2, 3, 4],
        title_key: "LAB_EXPERIMENT_SQLITE_VACUUM_TITLE",
        desc_key: "LAB_EXPERIMENT_SQLITE_VACUUM_DESCRIPTION",
    },
    ExperimentDef {
        id: "win32_empty_working_set",
        groups: &[1, 3],
        title_key: "LAB_EXPERIMENT_WIN32_EMPTY_WORKING_SET_TITLE",
        desc_key: "LAB_EXPERIMENT_WIN32_EMPTY_WORKING_SET_DESCRIPTION",
    },
    ExperimentDef {
        id: "async_banlist_apply",
        groups: &[0, 1, 2, 3, 4],
        title_key: "LAB_EXPERIMENT_ASYNC_BANLIST_APPLY",
        desc_key: "LAB_EXPERIMENT_ASYNC_BANLIST_APPLY_DESCRIPTION",
    },
    ExperimentDef {
        id: "async_ipdb_loading",
        groups: &[0, 1, 2, 3, 4],
        title_key: "LAB_EXPERIMENT_ASYNC_IPDB_LOADING",
        desc_key: "LAB_EXPERIMENT_ASYNC_IPDB_LOADING_DESCRIPTION",
    },
];

/// Java `String.hashCode()`：按 UTF-16 码元迭代（`h*31 + c`，i32 环绕）。
fn java_hashcode(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

/// 实验组号（0..4），对齐上游 `Math.abs(hashCode % 5)`。
fn experimental_group(installation_id: &str) -> i32 {
    (java_hashcode(installation_id) % 5).abs()
}

fn read_lab(path: &StdPath) -> serde_yaml::Mapping {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Mapping>(&text).ok())
        .unwrap_or_default()
}

fn write_lab(path: &StdPath, map: &serde_yaml::Mapping) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_yaml::to_string(map).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn lab_enabled(map: &serde_yaml::Mapping) -> bool {
    map.get(serde_yaml::Value::String("enabled".into()))
        .and_then(|v| v.as_bool())
        // 上游首次创建配置文件时写 `enabled: true`
        .unwrap_or(true)
}

/// 对齐 `Laboratory.isExperimentActivated`：总开关关闭时恒 false；`"default"` 看实验组。
fn is_activated(def: &ExperimentDef, map: &serde_yaml::Mapping, group: i32) -> bool {
    if !lab_enabled(map) {
        return false;
    }
    let by_group = def.groups.contains(&group);
    match map.get(serde_yaml::Value::String(def.id.into())) {
        None => by_group,
        Some(v) => match v {
            serde_yaml::Value::String(s) if s == "default" => by_group,
            other => other.as_bool().unwrap_or(by_group),
        },
    }
}

fn disabled() -> Response {
    (
        StatusCode::NOT_FOUND,
        crate::std_resp(false, Some("LABORATORY_DISABLED"), Value::Null),
    )
        .into_response()
}

/// `GET /api/laboratory/experiments`：实验清单与各自激活状态。
pub async fn experiments(State(state): State<AppState>) -> Response {
    let Some(path) = state.lab_path.as_ref() else {
        return disabled();
    };
    let map = read_lab(path);
    let group = experimental_group(&state.backend.installation_id());
    let enabled = lab_enabled(&map);
    let items: Vec<Value> = EXPERIMENTS
        .iter()
        .map(|def| {
            json!({
                "id": def.id,
                "title": state
                    .translator
                    .render(&TranslationComponent::new(def.title_key), &state.locale),
                "description": state
                    .translator
                    .render(&TranslationComponent::new(def.desc_key), &state.locale),
                "activated": is_activated(def, &map, group),
            })
        })
        .collect();
    (
        StatusCode::OK,
        crate::std_resp(
            true,
            None,
            json!({ "labEnabled": enabled, "experiments": items }),
        ),
    )
        .into_response()
}

/// `PUT /api/laboratory/experiment/{id}`：设置单个实验开关（body: `{status}`）。
pub async fn set_experiment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(path) = state.lab_path.as_ref() else {
        return disabled();
    };
    let Some(def) = EXPERIMENTS.iter().find(|d| d.id == id) else {
        return (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some("LAB_EXPERIMENT_NOT_FOUND"), Value::Null),
        )
            .into_response();
    };
    let status = body.get("status").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut map = read_lab(path);
    map.insert(serde_yaml::Value::String(def.id.into()), serde_yaml::Value::Bool(status));
    match write_lab(path, &map) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `POST /api/laboratory/config`：设置实验总开关（body: `{enabled}`）。
pub async fn set_config(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(path) = state.lab_path.as_ref() else {
        return disabled();
    };
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut map = read_lab(path);
    map.insert(
        serde_yaml::Value::String("enabled".into()),
        serde_yaml::Value::Bool(enabled),
    );
    match write_lab(path, &map) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}
