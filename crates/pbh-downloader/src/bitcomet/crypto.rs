//! BitComet WebUI 登录凭据加密，忠实复刻上游 `impl/bitcomet/crypto/BCAESTool`。
//!
//! 上游 `credential(json, cid)` 产出一段自描述二进制（**无版本号**，头部为固定魔数 `03 01`）：
//!
//! ```text
//! msg = 0x03 || 0x01 || t(8) || r(8) || iv(16) || AES-256-CBC/PKCS5Padding(json) || HmacSHA256(msg)
//! ```
//!
//! - `t` / `r`：`SecureRandom` 生成的 8 字节盐；
//! - `iv`：`Cipher.init(ENCRYPT_MODE, keySpec)` 由 JCE 随机生成的 16 字节 IV；
//! - AES 密钥 `n = PBKDF2WithHmacSHA1(cid, t, 10000, 32)`（32 字节 → **AES-256**）；
//! - HMAC 密钥 `i = PBKDF2WithHmacSHA1(cid, r, 10000, 32)`；
//! - 最终返回整段消息（含 32 字节 HMAC）的 Base64。
//!
//! 说明：Java 侧 `PBEKeySpec` 的密码是 `cid` 字符串的 `char[]`，JCE 按 UTF-8 转字节；
//! 本实现直接取 `cid.as_bytes()`（`cid` 为 UUID 字符串，两者等价）。

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;
use sha2::Sha256;

/// `UUID.nameUUIDFromBytes("PeerBanHelper".getBytes(UTF_8))`：上游 `BitComet.clientId`
/// （UUID v3：`MD5("PeerBanHelper")` 后写入版本位 `3` 与变体位 `10`）。
pub const CLIENT_ID: &str = "78ec56a2-8d0c-31ca-a843-f49fc1f20153";

/// PBKDF2 迭代次数（`new PBEKeySpec(..., 10000, 32 * 8)`）。
const PBKDF2_ROUNDS: u32 = 10_000;
/// PBKDF2 派生密钥长度（字节）：32 → AES-256。
const KEY_LEN: usize = 32;
/// `t` / `r` 盐长度（`new byte[8]`）。
const SALT_LEN: usize = 8;
/// AES-CBC IV 长度。
const IV_LEN: usize = 16;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// 生成登录凭据，对齐 `BCAESTool.credential`：每次调用都使用全新的随机 `t`/`r`/`iv`。
pub fn credential(json: &str, cid: &str) -> String {
    let mut t = [0u8; SALT_LEN];
    let mut r = [0u8; SALT_LEN];
    let mut iv = [0u8; IV_LEN];
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut t);
    rng.fill_bytes(&mut r);
    rng.fill_bytes(&mut iv);
    credential_with(json, cid, &t, &r, &iv)
}

/// 与 [`credential`] 相同，但随机量由调用方给出（供已知向量测试固定输入）。
pub fn credential_with(json: &str, cid: &str, t: &[u8; SALT_LEN], r: &[u8; SALT_LEN], iv: &[u8; IV_LEN]) -> String {
    let aes_key = pbkdf2_sha1(cid.as_bytes(), t);
    let hmac_key = pbkdf2_sha1(cid.as_bytes(), r);

    let ciphertext = Aes256CbcEnc::new_from_slices(&aes_key, iv)
        .expect("AES-256 密钥/IV 长度固定，构造不会失败")
        .encrypt_padded_vec_mut::<Pkcs7>(json.as_bytes());

    let mut msg = Vec::with_capacity(2 + SALT_LEN * 2 + IV_LEN + ciphertext.len() + 32);
    msg.push(3); // m[0]
    msg.push(1); // m[1]
    msg.extend_from_slice(t);
    msg.extend_from_slice(r);
    msg.extend_from_slice(iv);
    msg.extend_from_slice(&ciphertext);

    let mut mac = <HmacSha256 as Mac>::new_from_slice(&hmac_key)
        .expect("HMAC 密钥长度任意，构造不会失败");
    mac.update(&msg);
    msg.extend_from_slice(&mac.finalize().into_bytes());

    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(msg)
}

/// `PBKDF2WithHmacSHA1(cid, salt, 10000, 32)`。
fn pbkdf2_sha1(password: &[u8], salt: &[u8]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, PBKDF2_ROUNDS, &mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_matches_java_name_uuid_from_bytes() {
        assert_eq!(CLIENT_ID, "78ec56a2-8d0c-31ca-a843-f49fc1f20153");
    }

    /// 已知向量：`t = 00..07`、`r = 08..0f`、`iv = 10..1f`，
    /// 期望值由 Java 算法（PBKDF2WithHmacSHA1 + AES-256-CBC/PKCS5Padding + HmacSHA256）独立复算得出。
    #[test]
    fn credential_matches_upstream_layout_for_fixed_randomness() {
        let t = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let r = [8u8, 9, 10, 11, 12, 13, 14, 15];
        let iv = [16u8, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
        let credential = credential_with(
            r#"{"username":"admin","password":"adminadmin"}"#,
            CLIENT_ID,
            &t,
            &r,
            &iv,
        );
        assert_eq!(
            credential,
            "AwEAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eH0odpdUEvupBuEtD1tQdYzlF3JixP6jpK9/TEsKjXx4q6MaSt8b84ltcwBWHwgk1XZpiv4/cS+PoY0h6FzsHL4GhHC1ifcRb5GPTg7qS+vgt"
        );
    }

    /// 头部魔数与消息长度：`03 01 | t(8) | r(8) | iv(16) | 密文 | hmac(32)`。
    #[test]
    fn credential_prefix_is_fixed_magic_and_salts() {
        use base64::Engine as _;
        let t = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let r = [8u8, 9, 10, 11, 12, 13, 14, 15];
        let iv = [16u8; 16];
        let raw = base64::engine::general_purpose::STANDARD
            .decode(credential_with("{}", CLIENT_ID, &t, &r, &iv))
            .unwrap();
        assert_eq!(&raw[..2], &[3, 1]);
        assert_eq!(&raw[2..10], &t);
        assert_eq!(&raw[10..18], &r);
        assert_eq!(&raw[18..34], &iv);
        // "{}" 经 PKCS5 补齐到 16 字节 → 2 + 8 + 8 + 16 + 16 + 32
        assert_eq!(raw.len(), 2 + 8 + 8 + 16 + 16 + 32);
    }

    #[test]
    fn pbkdf2_derives_thirty_two_bytes() {
        // 与 Python hashlib.pbkdf2_hmac("sha1", cid, t, 10000, 32) 对照
        let t = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let key = pbkdf2_sha1(CLIENT_ID.as_bytes(), &t);
        assert_eq!(
            key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "a6d19d2fb6c2080851e25033b3d8032ce719de09f97b2fca160cd2725636a941"
        );
    }

    #[test]
    fn random_credentials_are_unique() {
        let a = credential("{}", CLIENT_ID);
        let b = credential("{}", CLIENT_ID);
        assert_ne!(a, b, "t/r/iv 每次随机，输出不应相同");
    }
}
