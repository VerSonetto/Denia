//! 凭据原语:随机令牌、哈希存储、常量时间比较。
//!
//! 三条纪律在这里落地,调用方不需要各自记一遍:
//! 1. 令牌一律 32 字节 CSPRNG(`OsRng`,不是 `thread_rng` —— 后者是可预测的
//!    PRNG,种子被拿到就能重放);
//! 2. 服务端只存 `sha256` 十六进制,明文只出现在响应体与二维码里;
//! 3. 比对密钥材料一律走常量时间,不因为"差一位提前返回"泄漏前缀。

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 32 字节 = 256 bit 熵,base64url 后 43 字符。
const TOKEN_BYTES: usize = 32;

/// 生成一个新令牌(明文)。只应出现在响应体、二维码与终端提示里。
pub fn new_token() -> String {
    let mut buffer = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

/// 令牌的存储形态:sha256 十六进制。查表即按此键,明文永不入表。
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 6 位数字 PIN(带前导零)。用 `OsRng` 均匀取样,不用时间/进程号之类的
/// 可猜来源。
pub fn new_pin() -> String {
    let value = rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..1_000_000u32);
    format!("{value:06}")
}

/// 新会话/挑战的标识符(uuid v4,只作标识,不作凭据)。
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 常量时间字节比较:长度不同直接判否(长度本身不是秘密),长度相同时
/// 累积全部差异再判定。
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b, "两次生成的令牌不能相同");
        assert_eq!(a.len(), 43, "32 字节 base64url 无填充是 43 字符");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "令牌必须能直接放进 URL query 与 cookie:{a}"
        );
    }

    #[test]
    fn hash_is_stable_and_hides_plaintext() {
        let token = new_token();
        let hash = hash_token(&token);
        assert_eq!(hash.len(), 64, "sha256 十六进制是 64 字符");
        assert_eq!(hash, hash_token(&token), "同一令牌哈希必须稳定");
        assert!(!hash.contains(&token), "哈希里不得包含明文");
        assert_ne!(hash, hash_token(&new_token()));
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..64 {
            let pin = new_pin();
            assert_eq!(pin.len(), 6, "{pin}");
            assert!(pin.chars().all(|c| c.is_ascii_digit()), "{pin}");
        }
    }

    #[test]
    fn constant_time_compare_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
