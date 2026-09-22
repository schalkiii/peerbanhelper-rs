//! Aria2Next RPC 请求/响应 DTO，逐字对齐上游：
//! - `util/jsonrpc/JsonRpcRequest`、`util/jsonrpc/JsonRpcResponse`（字段与声明顺序均一致）
//! - `downloader/impl/aria2next/bean/*`（`A2Task` 及其内嵌 `BittorrentType`/`InfoType`/
//!   `FilesType`、`A2Peer`、`A2Version`、`A2SetBtPeerBlocklist`）
//!
//! **宽松类型转换**：aria2 把数字与布尔值都编码为 JSON **字符串**（`"totalLength":"1234"`、
//! `"seeder":"true"`），上游用 Gson 反序列化（`JsonUtil.standard()`），Gson 的 `JsonReader`
//! 会自动做字符串↔数字/布尔的强转。本实现用 [`lenient`] 中的 `deserialize_with` 复刻：
//! - `nextLong` / `nextInt`：数字或字符串皆可；字符串先按 `Long.parseLong`，失败再按
//!   `Double.parseDouble` 取整（非整数值报 `NumberFormatException`）；基本类型字段遇到
//!   JSON `null` → `0`。
//! - `nextBoolean`：JSON 布尔，或字符串按 `Boolean.parseBoolean` 语义（仅 `"true"`、
//!   大小写不敏感；`"false"`/`"1"` 等一律为 `false`）；`null` → `false`。
//! - `nextDouble`：数字或字符串（`Double.parseDouble`，允许首尾空白）；`null` → `0.0`。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 对齐 `util/jsonrpc/JsonRpcRequest`：`jsonrpc` 固定 `"2.0"`，`id` 为随机 UUID 字符串。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: String,
    pub method: String,
    pub params: Vec<Value>,
}

/// 对齐 `util/jsonrpc/JsonRpcResponse<T>`。
///
/// 四个字段都是 `Option`：缺失与显式 `null` 都读成 `None`（`serde` 对 `Option` 的默认行为，
/// 不加 `#[serde(default)]` 以免给泛型 `T` 引入多余的 `Default` 约束）。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: Option<String>,
    /// Gson 把数字 id 也读成 `String`，故这里不限定类型。
    pub id: Option<Value>,
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
}

/// 对齐 `util/jsonrpc/JsonRpcResponse.JsonRpcError`。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    #[serde(default, deserialize_with = "lenient::i64")]
    pub code: i64,
    #[serde(default)]
    pub message: Option<String>,
}

/// 对齐 `bean/A2Version`（`aria2.getVersion` 的 result）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2Version {
    #[serde(default)]
    pub enabled_features: Option<Vec<String>>,
    /// 仅 Aria2Next 分支返回 `"aria2-next"`。
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub rpc_version: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 对齐 `bean/A2SetBtPeerBlocklist`（`aria2.setBtPeerBlocklist` 的 result）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2SetBtPeerBlocklist {
    #[serde(default, deserialize_with = "lenient::option_i32")]
    pub disconnected_peers: Option<i32>,
    #[serde(default, deserialize_with = "lenient::option_i32")]
    pub removed_peers: Option<i32>,
    #[serde(default, deserialize_with = "lenient::option_i32")]
    pub revision: Option<i32>,
    #[serde(default, deserialize_with = "lenient::option_i32")]
    pub rule_count: Option<i32>,
}

/// 对齐 `bean/A2Task`（`aria2.tellActive`/`tellWaiting`/`tellStopped` 的元素）。
///
/// 只声明上游 DTO 里存在的字段：请求的 `keys` 数组更宽（含 `pieceLength`、`numPieces`、
/// `dir`、`verifiedLength` 等），但 Gson 会忽略这些未被声明的字段。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2Task {
    #[serde(default)]
    pub bittorrent: Option<A2Bittorrent>,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub completed_length: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub connections: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub download_speed: i64,
    #[serde(default)]
    pub files: Option<Vec<A2File>>,
    #[serde(default)]
    pub gid: Option<String>,
    #[serde(default)]
    pub info_hash: Option<String>,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub num_seeders: i64,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub seeder: bool,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub total_length: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub upload_speed: i64,
}

/// 对齐 `A2Task.BittorrentType`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2Bittorrent {
    #[serde(default)]
    pub announce_list: Option<Vec<Vec<String>>>,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub creation_date: i64,
    #[serde(default)]
    pub info: Option<A2BittorrentInfo>,
    #[serde(default)]
    pub magnet_link: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

/// 对齐 `A2Task.InfoType`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct A2BittorrentInfo {
    #[serde(default)]
    pub name: Option<String>,
}

/// 对齐 `A2Task.FilesType`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2File {
    #[serde(default, deserialize_with = "lenient::i64")]
    pub completed_length: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub index: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub length: i64,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub selected: bool,
    #[serde(default)]
    pub uris: Option<Vec<Value>>,
}

/// 对齐 `bean/A2Peer`（`aria2.getPeers` 的元素）。
///
/// 全部字段都保留（与上游 DTO 一致），但其中 `bitfield`、`flags`（aria2 的 flags 字符串，
/// 上游被同名的 `getFlags(): PeerFlag` 遮蔽，实际从未读取）、`completed_length`、
/// `handshaking`（渲染进 `PeerData.flags` 的 `libtorrent` 风格字符串无法表达）不参与映射。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2Peer {
    #[serde(default, deserialize_with = "lenient::bool")]
    pub am_choking: bool,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub am_interested: bool,
    #[serde(default)]
    pub bitfield: Option<String>,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub completed_length: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub download_speed: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub downloaded: i64,
    #[serde(default)]
    pub flags: Option<String>,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub handshaking: bool,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub incoming: bool,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub optimistic_unchoke: bool,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub peer_choking: bool,
    #[serde(default)]
    pub peer_client_name: Option<String>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub peer_interested: bool,
    #[serde(default, deserialize_with = "lenient::i32")]
    pub port: i32,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub progress: f64,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub seeder: bool,
    #[serde(default, deserialize_with = "lenient::bool")]
    pub snubbed: bool,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub upload_speed: i64,
    #[serde(default, deserialize_with = "lenient::i64")]
    pub uploaded: i64,
}

/// Gson `JsonReader` 的宽松标量转换（见模块文档）。
mod lenient {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// 对齐 `JsonReader.nextLong()`（含原始类型字段遇 `null` → `0`）。
    pub fn i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let value = Value::deserialize(deserializer)?;
        to_i64(&value).map_err(D::Error::custom)
    }

    /// 对齐 `JsonReader.nextInt()`。
    pub fn i32<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
        let value = Value::deserialize(deserializer)?;
        to_i64(&value)
            .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
            .map_err(D::Error::custom)
    }

    /// 装箱 `Integer` 字段：`null` 保留为 `None`（Gson 只对**基本类型**把 `null` 归零）。
    pub fn option_i32<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i32>, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(None);
        }
        to_i64(&value)
            .map(|v| Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32))
            .map_err(D::Error::custom)
    }

    /// 对齐 `JsonReader.nextDouble()`。
    pub fn f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        let value = Value::deserialize(deserializer)?;
        to_f64(&value).map_err(D::Error::custom)
    }

    /// 对齐 `JsonReader.nextBoolean()`。
    pub fn bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
        let value = Value::deserialize(deserializer)?;
        to_bool(&value).map_err(D::Error::custom)
    }

    fn to_i64(value: &Value) -> Result<i64, String> {
        match value {
            Value::Null => Ok(0),
            Value::Number(number) => {
                if let Some(v) = number.as_i64() {
                    Ok(v)
                } else if let Some(v) = number.as_u64() {
                    Ok(v as i64)
                } else {
                    integer_of(number.as_f64().unwrap_or_default(), &number.to_string())
                }
            }
            Value::String(raw) => match raw.parse::<i64>() {
                Ok(v) => Ok(v),
                // `Double.parseDouble` 允许首尾空白
                Err(_) => match raw.trim().parse::<f64>() {
                    Ok(v) => integer_of(v, raw),
                    Err(_) => Err(format!("Expected a long but was {raw}")),
                },
            },
            other => Err(format!("Expected a long but was {other}")),
        }
    }

    /// `(long) asDouble` 且精度必须无损失，否则 Gson 抛 `NumberFormatException`。
    fn integer_of(as_f64: f64, raw: &str) -> Result<i64, String> {
        let as_i64 = as_f64 as i64;
        if as_i64 as f64 == as_f64 {
            Ok(as_i64)
        } else {
            Err(format!("Expected a long but was {raw}"))
        }
    }

    fn to_f64(value: &Value) -> Result<f64, String> {
        match value {
            Value::Null => Ok(0.0),
            Value::Number(number) => number
                .as_f64()
                .ok_or_else(|| format!("Expected a double but was {number}")),
            Value::String(raw) => raw
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("Expected a double but was {raw}")),
            other => Err(format!("Expected a double but was {other}")),
        }
    }

    fn to_bool(value: &Value) -> Result<bool, String> {
        match value {
            Value::Null => Ok(false),
            Value::Bool(v) => Ok(*v),
            // `Boolean.parseBoolean`：仅 "true"（大小写不敏感）为真
            Value::String(raw) => Ok(raw.eq_ignore_ascii_case("true")),
            other => Err(format!("Expected a boolean but was {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aria2 把数字与布尔都编码为字符串。
    #[test]
    fn task_accepts_string_encoded_scalars() {
        let task: A2Task = serde_json::from_str(
            r#"{
              "gid": "g1",
              "status": "active",
              "totalLength": "1234",
              "completedLength": "42",
              "downloadSpeed": "7",
              "uploadSpeed": "8",
              "numSeeders": "3",
              "seeder": "false",
              "connections": "9",
              "infoHash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "bittorrent": { "info": { "name": "ubuntu.iso" } }
            }"#,
        )
        .unwrap();
        assert_eq!(task.total_length, 1234);
        assert_eq!(task.completed_length, 42);
        assert_eq!(task.download_speed, 7);
        assert_eq!(task.upload_speed, 8);
        assert_eq!(task.num_seeders, 3);
        assert!(!task.seeder);
        assert_eq!(task.connections, 9);
        assert_eq!(task.gid.as_deref(), Some("g1"));
    }

    /// 原生数字/布尔（JSON-RPC 兼容实现）与 `null` 都要能读。
    #[test]
    fn scalars_accept_native_json_and_null() {
        let task: A2Task = serde_json::from_str(
            r#"{ "totalLength": 100, "completedLength": null, "seeder": true }"#,
        )
        .unwrap();
        assert_eq!(task.total_length, 100);
        assert_eq!(task.completed_length, 0);
        assert!(task.seeder);

        let peer: A2Peer = serde_json::from_str(
            r#"{ "amChoking": "true", "peerChoking": "false", "port": "6881",
                 "progress": "0.5", "incoming": null }"#,
        )
        .unwrap();
        assert!(peer.am_choking);
        assert!(!peer.peer_choking);
        assert_eq!(peer.port, 6881);
        assert_eq!(peer.progress, 0.5);
        assert!(!peer.incoming);
    }

    /// Gson 的 `"1.0"` → `1` 与 `"1.5"` → 报错语义。
    #[test]
    fn long_accepts_integral_strings_only() {
        let ok: A2Task = serde_json::from_str(r#"{ "totalLength": "1.0" }"#).unwrap();
        assert_eq!(ok.total_length, 1);
        assert!(serde_json::from_str::<A2Task>(r#"{ "totalLength": "1.5" }"#).is_err());
        assert!(serde_json::from_str::<A2Task>(r#"{ "totalLength": "abc" }"#).is_err());
    }

    /// `Boolean.parseBoolean` 语义：只有 "true" 为真。
    #[test]
    fn boolean_follows_parse_boolean() {
        let task: A2Task =
            serde_json::from_str(r#"{ "seeder": "TRUE", "completedLength": "1" }"#).unwrap();
        assert!(task.seeder);
        let task: A2Task = serde_json::from_str(r#"{ "seeder": "1" }"#).unwrap();
        assert!(!task.seeder);
    }

    /// JSON-RPC 信封：`error` 与 `result` 都允许为 `null`。
    #[test]
    fn rpc_envelope_parses_null_fields() {
        let resp: JsonRpcResponse<A2Version> = serde_json::from_str(
            r#"{ "jsonrpc": "2.0", "id": "1", "result": null, "error": null }"#,
        )
        .unwrap();
        assert!(resp.result.is_none());
        assert!(resp.error.is_none());

        let resp: JsonRpcResponse<A2Version> = serde_json::from_str(
            r#"{ "jsonrpc": "2.0", "id": 7,
                "error": { "code": 1, "message": "Unauthorized" } }"#,
        )
        .unwrap();
        let error = resp.error.unwrap();
        assert_eq!(error.code, 1);
        assert_eq!(error.message.as_deref(), Some("Unauthorized"));

        let resp: JsonRpcResponse<A2Version> = serde_json::from_str(
            r#"{ "result": { "product": "aria2-next", "version": "1.37.0", "rpcVersion": "1.3.0" } }"#,
        )
        .unwrap();
        let version = resp.result.unwrap();
        assert_eq!(version.product.as_deref(), Some("aria2-next"));
        assert_eq!(version.version.as_deref(), Some("1.37.0"));
    }
}
