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

/// 前端 `netTypeArray` 的 9 个取值 ↔ `profile.yml` 的 kebab 开关键。
///
/// 注意 `datacenter`：上游 `ProfileUpdateScript` 迁移时写成 `dataCenter`，而 WebUI
/// 的 `netTypeArray` 用的是 `datacenter`（`IPBlackList#checkIPDB` 两种都接受）。
/// 这里统一以 WebUI 的写法为准（`dataCenter` 作为输入别名），否则设置页勾选
/// 「数据中心」后读写两侧对不上。
const NET_TYPE_KEYS: [(&str, &str); 9] = [
    ("wideband", "wideband"),
    ("baseStation", "base-station"),
    ("governmentAndEnterpriseLine", "government-and-enterprise-line"),
    ("businessPlatform", "business-platform"),
    ("backboneNetwork", "backbone-network"),
    ("ipPrivateNetwork", "ip-private-network"),
    ("internetCafe", "internet-cafe"),
    ("iot", "iot"),
    ("datacenter", "datacenter"),
];

fn normalize_net_type(token: &str) -> String {
    match token {
        "dataCenter" => "datacenter".to_string(),
        other => other.to_string(),
    }
}

/// 读取 `net-type`：兼容 kebab 布尔映射（出厂 profile.yml）与字符串列表（上游迁移后）两种形态。
fn read_net_types(section: &Value) -> Vec<String> {
    let Some(value) = section
        .get("net_type")
        .or_else(|| section.get("net-type"))
    else {
        return Vec::new();
    };
    match value {
        Value::Object(flags) => NET_TYPE_KEYS
            .iter()
            .filter(|(_, key)| flags.get(*key).and_then(|v| v.as_bool()).unwrap_or(false))
            .map(|(token, _)| (*token).to_string())
            .collect(),
        Value::Array(list) => list
            .iter()
            .filter_map(|v| v.as_str())
            .map(normalize_net_type)
            .filter(|token| NET_TYPE_KEYS.iter().any(|(known, _)| known == token))
            .collect(),
        _ => Vec::new(),
    }
}

/// 写入 `net-type`：统一落 kebab 布尔映射（配置解析两种形态都接受）。
fn write_net_types(section: &mut Value, selected: &[String]) {
    let selected: Vec<String> = selected.iter().map(|s| normalize_net_type(s)).collect();
    let flags: serde_json::Map<String, Value> = NET_TYPE_KEYS
        .iter()
        .map(|(token, key)| ((*key).to_string(), Value::Bool(selected.iter().any(|s| s == token))))
        .collect();
    section["net_type"] = Value::Object(flags);
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
        // netType 以 WebUI 的 camelCase 集合语义暴露（兼容 map / list 两种配置形态）
        "netType" => Value::Array(
            read_net_types(&section)
                .into_iter()
                .map(Value::String)
                .collect(),
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
        // 数组 → kebab 布尔映射（9 个键，含 datacenter）；兼容 `dataCenter` 别名
        let selected: Vec<String> = incoming
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        write_net_types(&mut section, &selected);
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
/// （对齐上游 `UserIPTestResult(from, to, generatedCidr, count)`）。
pub async fn test_ip(Json(body): Json<Value>) -> Response {
    let Some(ip_str) = body.get("ip").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some("缺少 ip 字段"), Value::Null),
        )
            .into_response();
    };
    match parse_ip_test(ip_str) {
        Some((from, to, generated_cidr, count)) => (
            StatusCode::OK,
            crate::std_resp(
                true,
                None,
                json!({
                    "from": from,
                    "to": to,
                    "generatedCidr": generated_cidr,
                    "count": count,
                }),
            ),
        )
            .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            crate::std_resp(false, Some("IP_BLACKLIST_PUT_IP_INVALID_IP"), Value::Null),
        )
            .into_response(),
    }
}

/// 解析单 IP / CIDR 的 `(from, to, generatedCidr, count)`。
///
/// 上游 `IPAddressUtil.getIPAddress` 同时接受单 IP 与 CIDR；`getCount()` 的口径是
/// 该地址族的**地址总数**（IPv4 /24 = 256；IPv6 /64 = 2^64）。
fn parse_ip_test(ip_str: &str) -> Option<(String, String, String, String)> {
    if let Ok(net) = ip_str.parse::<ipnet::IpNet>() {
        let lower = net.network();
        let upper = net.broadcast();
        let bits = match lower {
            std::net::IpAddr::V4(_) => 32u32,
            std::net::IpAddr::V6(_) => 128u32,
        };
        let host_bits = bits - u32::from(net.prefix_len());
        let count = if host_bits >= 128 {
            // 2^128（IPv6 /0）超出 u128 上限
            "340282366920938463463374607431768211456".to_string()
        } else {
            (1u128 << host_bits).to_string()
        };
        // 上游 `toCompressedString()` 输出的是前缀块（主机位清零），不是输入的原始写法
        let generated_cidr = ipnet::IpNet::new(lower, net.prefix_len())
            .map(|block| block.to_string())
            .unwrap_or_else(|_| net.to_string());
        return Some((lower.to_string(), upper.to_string(), generated_cidr, count));
    }
    ip_str.parse::<std::net::IpAddr>().ok().map(|addr| {
        (
            addr.to_string(),
            addr.to_string(),
            addr.to_string(),
            "1".to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_test_accepts_single_host_and_counts_ipv4_hosts() {
        assert_eq!(
            parse_ip_test("1.2.3.4"),
            Some(("1.2.3.4".into(), "1.2.3.4".into(), "1.2.3.4".into(), "1".into()))
        );
        assert_eq!(
            parse_ip_test("1.2.3.4/24"),
            Some((
                "1.2.3.0".into(),
                "1.2.3.255".into(),
                "1.2.3.0/24".into(),
                "256".into()
            ))
        );
        assert_eq!(
            parse_ip_test("::1"),
            Some(("::1".into(), "::1".into(), "::1".into(), "1".into()))
        );
    }

    #[test]
    fn net_type_reads_both_config_forms_and_uses_webui_tokens() {
        let flags = json!({ "net-type": { "wideband": true, "datacenter": true } });
        assert_eq!(
            read_net_types(&flags),
            vec!["wideband".to_string(), "datacenter".to_string()]
        );
        let list = json!({ "net_type": ["baseStation", "dataCenter"] });
        assert_eq!(
            read_net_types(&list),
            vec!["baseStation".to_string(), "datacenter".to_string()]
        );
    }

    #[test]
    fn net_type_write_roundtrip_includes_datacenter() {
        let mut section = json!({});
        write_net_types(&mut section, &["datacenter".into(), "iot".into()]);
        assert_eq!(section["net_type"]["datacenter"], true);
        assert_eq!(section["net_type"]["iot"], true);
        assert_eq!(section["net_type"]["wideband"], false);
        assert_eq!(
            read_net_types(&section),
            vec!["iot".to_string(), "datacenter".to_string()]
        );
    }
}
