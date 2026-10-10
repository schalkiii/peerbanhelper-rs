//! IP 规则黑名单的 Web 管理端点（对齐上游 `IPBlackList` 模块自注册的路由）。
//!
//! 上游语义：设置页按 `ruleType` 读写 `profile.yml` 中 `module.ip-address-blocker`
//! 段的对应键（`ips/ports/asns/regions/cities/net-type`）；PUT 请求体为
//! `{<ruleType>: <值>}`（前端 `endpoint` 层统一构造），netType 为整体替换数组。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `ruleType` → `profile.yml` 段内的键。
fn profile_key(rule_type: &str) -> Option<&'static str> {
    match rule_type {
        "ip" => Some("ips"),
        "port" => Some("ports"),
        "asn" => Some("asns"),
        "region" => Some("regions"),
        "city" => Some("cities"),
        // read_config 会把 YAML 的 `-` 键 normalize 为 `_`（web 层键约定）
        "netType" => Some("net_type"),
        _ => None,
    }
}

/// 上游 `handleWebAPI`：响应键与 profile 键不同（`ips` → 响应 `ip` 等）。
fn response_key(rule_type: &str) -> Option<&'static str> {
    match rule_type {
        "ip" => Some("ip"),
        "port" => Some("port"),
        "asn" => Some("asn"),
        "region" => Some("region"),
        "city" => Some("city"),
        "netType" => Some("netType"),
        _ => None,
    }
}

fn read_section(state: &AppState) -> Result<Value, String> {
    let doc = state.backend.read_config("profile")?;
    Ok(doc
        .get("module")
        .and_then(|m| m.get("ip-address-blocker").or_else(|| m.get("ip_address_blocker")))
        .cloned()
        .unwrap_or(Value::Null))
}

fn write_section(state: &AppState, section: &Value) -> Result<(), String> {
    let mut doc = state.backend.read_config("profile")?;
    let Some(obj) = doc.get_mut("module").and_then(|m| m.as_object_mut()) else {
        return Err("profile.yml 缺少 module 段".into());
    };
    let key = if obj.contains_key("ip-address-blocker") {
        "ip-address-blocker"
    } else {
        "ip_address_blocker"
    };
    let Some(blocker) = obj.get_mut(key).and_then(|b| b.as_object_mut()) else {
        return Err("profile.yml 缺少 module.ip-address-blocker 段".into());
    };
    if let Some(map) = section.as_object() {
        for (k, v) in map {
            blocker.insert(k.clone(), v.clone());
        }
    }
    state.backend.write_config("profile", &doc)
}

/// `GET /api/modules/ipblacklist/{ruleType}`：读某类规则（响应 `{<键>: <列表>}`）。
pub async fn get_rule(State(state): State<AppState>, Path(rule_type): Path<String>) -> Response {
    let Some(resp_key) = response_key(&rule_type) else {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("Illegal pathParams: ruleType not acceptable."), Value::Null),
        )
            .into_response();
    };
    let section = match read_section(&state) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::std_resp(false, Some(&e), Value::Null),
            )
                .into_response()
        }
    };
    let value = match rule_type.as_str() {
        // netType 在 profile 中是对象（wideband 等 8 键布尔），上游以 Set<String>
        // 语义暴露：返回值为 true 的键名列表
        "netType" => Value::Array(
            section
                .get("net-type")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter(|(_, v)| v.as_bool().unwrap_or(false))
                        .map(|(k, _)| Value::String(k.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => section.get(profile_key(&rule_type).unwrap_or_default()).cloned().unwrap_or(Value::Array(vec![])),
    };
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!({ resp_key: value })),
    )
        .into_response()
}

/// `PUT /api/modules/ipblacklist/{kind}`：追加/整体替换规则。
/// body 为 `{<kind>: <值>}`；`netType` 为字符串数组（整体替换）。
pub async fn put_rule(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match mutate(&state, &kind, &body, true) {
        Ok(()) => (
            StatusCode::CREATED,
            crate::std_resp(true, Some("OPERATION_EXECUTE_SUCCESSFULLY"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/modules/ipblacklist/{kind}`：移除规则，body 同 PUT。
pub async fn delete_rule(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match mutate(&state, &kind, &body, false) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OPERATION_EXECUTE_SUCCESSFULLY"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

fn mutate(state: &AppState, kind: &str, body: &Value, add: bool) -> Result<(), String> {
    let Some(p_key) = profile_key(kind) else {
        return Err(format!("Illegal pathParams: {kind} not acceptable."));
    };
    // 提取值：body 为 `{<kind>: <值>}`；netType 为裸数组（整体替换）
    let incoming = if kind == "netType" {
        body.clone()
    } else {
        body.get(kind)
            .or_else(|| body.get(p_key))
            .cloned()
            .ok_or_else(|| format!("请求体缺少 {kind} 字段"))?
    };

    let mut section = read_section(state)?;

    if kind == "netType" {
        // 数组 → 对象（8 键布尔映射）：列表里出现的键为 true，其余 false
        let names = [
            "wideband",
            "base-station",
            "government-and-enterprise-line",
            "business-platform",
            "backbone-network",
            "ip-private-network",
            "internet-cafe",
            "iot",
        ];
        let selected: Vec<String> = incoming
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let obj: serde_json::Map<String, Value> = names
            .iter()
            .map(|k| (k.to_string(), Value::Bool(selected.iter().any(|s| s == k))))
            .collect();
        section[p_key] = Value::Object(obj);
        return write_section(state, &section);
    }

    let Some(list) = section.get_mut(p_key).and_then(|v| v.as_array_mut()) else {
        return Err(format!("profile.yml 的 {p_key} 不是列表"));
    };
    let items: Vec<Value> = match incoming {
        Value::Array(a) => a,
        v => vec![v],
    };
    for item in items {
        let v = item.clone();
        let existing = list.iter().position(|x| *x == v);
        if add {
            if existing.is_none() {
                list.push(v);
            }
        } else if let Some(pos) = existing {
            list.remove(pos);
        }
    }
    write_section(state, &section)
}

/// `POST /api/modules/ipblacklist/ip/test`：解析 IP/CIDR 并返回范围信息
/// （对齐上游 `UserIPTestResult(lower, upper, compressed, count)`）。
pub async fn test_ip(Json(body): Json<Value>) -> Response {
    let Some(ip_str) = body.get("ip").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some("缺少 ip 字段"), Value::Null),
        )
            .into_response();
    };
    match ip_str.parse::<ipnet::IpNet>() {
        Ok(net) => {
            let lower = net.network();
            let upper = net.broadcast();
            let count = net.prefix_len();
            (
                StatusCode::OK,
                crate::std_resp(
                    true,
                    None,
                    json!({
                        "from": lower.to_string(),
                        "to": upper.to_string(),
                        "generatedCidr": net.to_string(),
                        "count": format!("{}", 1u128 << (128 - count as u128)),
                    }),
                ),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some("IP_BLACKLIST_PUT_IP_INVALID_IP"), Value::Null),
        )
            .into_response(),
    }
}
