//! BTN Proof-of-Work 求解与 base64 编解码。

use super::*;

// --------------------------------------------------------------- PoW captcha

/// 上游 `PoWClient.solve` 的迭代上限。
///
/// 上游在 `resultFuture.get()` 上无限等待（多线程暴力搜索）；本移植为单线程且必须保证
/// 不会挂住调用方，故设置上界。达到上界时的失败路径与上游「求解异常」一致：
/// 记日志、不加 PoW 请求头、继续后续流程。
pub const POW_MAX_ITERATIONS: u64 = 20_000_000;

/// `submit_histories` 单轮最多翻多少页（纯防御：数据源异常时避免无限上报）。
pub(crate) const MAX_HISTORY_PAGES: usize = 200;

/// 上游 `PoWClient.hasLeadingZeroBits`：摘要的前 `bits` 位必须全为 0。
pub fn has_leading_zero_bits(hash: &[u8], bits: i32) -> bool {
    if bits <= 0 {
        return true;
    }
    let bits = bits as usize;
    let full_bytes = bits / 8;
    let remaining_bits = bits % 8;
    if hash.len() < full_bytes {
        return false;
    }
    if hash[..full_bytes].iter().any(|byte| *byte != 0) {
        return false;
    }
    if remaining_bits > 0 {
        let mask = 0xFFu8 << (8 - remaining_bits);
        hash.get(full_bytes)
            .map(|byte| byte & mask == 0)
            .unwrap_or(false)
    } else {
        true
    }
}

/// `PoWCaptchaData.algorithm` 支持的摘要算法（上游 `MessageDigest.getInstance(algorithm)`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowAlgorithm {
    Sha256,
    Sha512,
}

impl PowAlgorithm {
    /// 上游把服务端的算法名直接交给 `MessageDigest`；这里按 JCA 标准名（大小写/连字符不敏感）识别。
    pub fn parse(algorithm: &str) -> Option<Self> {
        let normalized: String = algorithm.chars().filter(|c| !c.is_whitespace()).collect();
        match normalized.to_ascii_uppercase().replace('-', "").as_str() {
            "SHA256" | "SHA_256" => Some(Self::Sha256),
            "SHA512" | "SHA_512" => Some(Self::Sha512),
            _ => None,
        }
    }

    pub(crate) fn digest(self, challenge: &[u8], nonce: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Self::Sha256 => {
                let mut hasher = sha2::Sha256::new();
                hasher.update(challenge);
                hasher.update(nonce);
                hasher.finalize().to_vec()
            }
            Self::Sha512 => {
                let mut hasher = sha2::Sha512::new();
                hasher.update(challenge);
                hasher.update(nonce);
                hasher.finalize().to_vec()
            }
        }
    }
}

/// 上游 `PoWClient.solve(challenge, difficultyBits, algorithm)`：
/// 从随机 8 字节 nonce 开始递增，直到 `<algorithm>(challenge || nonce)` 的前
/// `difficulty_bits` 位为零；返回该 nonce 的 8 字节大端表示（`ByteBuffer.putLong`）。
///
/// 算法名无法识别或达到 [`POW_MAX_ITERATIONS`] 时返回 `None`（调用方按上游
/// `catch (Throwable)` 的分支处理：记日志、跳过 PoW 请求头）。
pub fn solve_pow(challenge: &[u8], difficulty_bits: i32, algorithm: &str) -> Option<Vec<u8>> {
    let algorithm = PowAlgorithm::parse(algorithm)?;
    let mut nonce = random_nonce();
    for _ in 0..POW_MAX_ITERATIONS {
        let nonce_bytes = nonce.to_be_bytes();
        if has_leading_zero_bits(&algorithm.digest(challenge, &nonce_bytes), difficulty_bits) {
            return Some(nonce_bytes.to_vec());
        }
        nonce = nonce.wrapping_add(1);
    }
    None
}

/// 上游 `new SecureRandom().nextLong()`：非密码学强度的 64 位起点。
fn random_nonce() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        state = (now_millis() as u64) ^ 0x9E37_79B9_7F4A_7C15;
    }
    // xorshift64*
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    STATE.store(state, Ordering::Relaxed);
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 上游 `Base64.getEncoder().encodeToString(...)`。
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let group = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(group >> 18) as usize & 63] as char);
        out.push(ALPHABET[(group >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(group >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[group as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 上游 `Base64.getDecoder().decode(...)`；非法输入返回 `None`（上游 `IllegalArgumentException`）。
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let value: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !value.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 4 * 3);
    for chunk in value.chunks(4) {
        let mut group: u32 = 0;
        let mut available = 0usize;
        for (index, byte) in chunk.iter().enumerate() {
            let decoded = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => continue,
                _ => return None,
            } as u32;
            group |= decoded << (18 - index * 6);
            available += 1;
        }
        if available == 0 {
            continue;
        }
        out.push((group >> 16) as u8);
        if available > 2 {
            out.push((group >> 8) as u8);
        }
        if available > 3 {
            out.push(group as u8);
        }
    }
    Some(out)
}
