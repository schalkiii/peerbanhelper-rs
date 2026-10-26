//! BTN 通用小工具：gzip、标识哈希、URL 拼接与时间。

use super::*;

// ------------------------------------------------------------------ 工具

/// 单个脉冲执行结果（由 `BtnNetwork::run_ability` 返回，`sync_due` 统一落账）。
pub fn gzip_bytes(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload)?;
    Ok(encoder.finish()?)
}

/// 上游 `InfoHashUtil.getHashedIdentifier`：infohash 小写 → CRC32 十六进制盐 →
/// SHA-256(明文 + 盐) 的小写十六进制。Guava（`Hashing.crc32().toString()` /
/// `Hashing.sha256().toString()`）输出与 `{:08x}` / `{:02x}` 一致。
pub fn get_hashed_identifier(torrent_info_hash: &str) -> String {
    use sha2::Digest;

    let handled = torrent_info_hash.to_lowercase();
    let salt = format!("{:08x}", crc32fast::hash(handled.as_bytes()));
    let mut hasher = sha2::Sha256::new();
    hasher.update(handled.as_bytes());
    hasher.update(salt.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 上游 `URLEncoder.encode(str, UTF_8)` 的查询段编码（空格 → `+`，保留 `-_.*` 与字母数字）。
pub(crate) fn url_encode_query(input: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push('+'),
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// 上游 `URLUtil.appendUrl(endpoint, params)`：按需用 `?` / `&` 追加密钥值对（已编码值）。
pub(crate) fn append_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let mut url = endpoint.to_string();
    for (key, value) in params {
        if url.contains('?') {
            url.push('&');
        } else {
            url.push('?');
        }
        url.push_str(key);
        url.push('=');
        url.push_str(value);
    }
    url
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 上游 `ThreadLocalRandom.current().nextLong(bound)`：由 ability 键推导的确定性伪随机
/// 初始延迟（避免测试中的不确定性）；`bound <= 0` 时为 0。
pub(crate) fn pseudo_random(seed: &str, bound: i64) -> i64 {
    if bound <= 0 {
        return 0;
    }
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01B3);
    }
    (hash % bound as u64) as i64
}
