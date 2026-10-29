//! 微信入站图片:CDN 下载、AES-128-ECB 解密与 MIME 识别。
//!
//! - 优先用 `media.encrypt_query_param` 构造 CDN 下载地址;否则用 `full_url`;
//! - `image_item.aeskey` 是 hex,`media.aes_key` 是 base64(可能是 16 字节原始 key,
//!   也可能是 hex 文本);
//! - 微信图片实际多为 JPEG,但按 magic bytes 识别更稳。

use aes::Aes128;
use aes::cipher::{Block, BlockDecrypt, KeyInit};
use base64::Engine;

use super::protocol::ImageItem;

/// 微信 CDN 的基础地址。
pub const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// 允许下载的 CDN 主机(防止服务端被诱导去访问任意 URL)。
const CDN_ALLOWLIST: &[&str] = &[
    "novac2c.cdn.weixin.qq.com",
    "ilinkai.weixin.qq.com",
    "wx.qlogo.cn",
    "thirdwx.qlogo.cn",
    "res.wx.qq.com",
    "mmbiz.qpic.cn",
    "mmbiz.qlogo.cn",
];

/// 单张图片的解密后上限:超过直接拒绝,避免一条消息把内存吃满。
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// 下载并解密后的图片。
#[derive(Debug, Clone)]
pub struct DownloadedImage {
    pub data: Vec<u8>,
    pub mime: String,
}

/// 生成下载地址。
pub fn image_url(image: &ImageItem) -> Result<String, String> {
    let media = image.media.as_ref();
    if let Some(param) = media
        .and_then(|media| media.encrypt_query_param.as_deref())
        .map(str::trim)
        .filter(|param| !param.is_empty())
    {
        return Ok(format!(
            "{}/download?encrypted_query_param={}",
            CDN_BASE_URL,
            encode_query(param)
        ));
    }
    if let Some(full_url) = media
        .and_then(|media| media.full_url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        ensure_allowed_url(full_url)?;
        return Ok(full_url.to_string());
    }
    if let Some(url) = image
        .url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        ensure_allowed_url(url)?;
        return Ok(url.to_string());
    }
    Err("图片消息缺少下载地址".to_string())
}

/// 解密图片。没有 key 时视为明文(部分消息不走 CDN 加密)。
pub fn decrypt_image(image: &ImageItem, raw: &[u8]) -> Result<Vec<u8>, String> {
    match image_key(image)? {
        Some(key) => aes128_ecb_decrypt(raw, &key),
        None => Ok(raw.to_vec()),
    }
}

/// 解析图片的 AES key:优先 `image_item.aeskey`(hex),其次 `media.aes_key`(base64)。
pub fn image_key(image: &ImageItem) -> Result<Option<[u8; 16]>, String> {
    if let Some(hex) = image
        .aeskey
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(parse_hex_key(hex)?));
    }
    if let Some(encoded) = image
        .media
        .as_ref()
        .and_then(|media| media.aes_key.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(parse_aes_key(encoded)?));
    }
    Ok(None)
}

/// 解析 base64 形式的 AES key:16 字节直接用;32 字节视为 hex 文本。
pub fn parse_aes_key(encoded: &str) -> Result<[u8; 16], String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("图片 AES key 不是合法 base64:{error}"))?;
    match decoded.len() {
        16 => Ok(decoded
            .try_into()
            .expect("长度已由 match 保证为 16")),
        32 => {
            let text = std::str::from_utf8(&decoded)
                .map_err(|_| "图片 AES key 文本不是合法 UTF-8".to_string())?;
            parse_hex_key(text)
        }
        other => Err(format!("无法识别的图片 AES key 长度:{other}")),
    }
}

fn parse_hex_key(text: &str) -> Result<[u8; 16], String> {
    if text.len() != 32 {
        return Err(format!(
            "图片 AES key 十六进制长度应为 32,实际 {}",
            text.len()
        ));
    }
    let mut key = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let part = std::str::from_utf8(chunk)
            .map_err(|_| "图片 AES key 含非法十六进制字符".to_string())?;
        key[index] = u8::from_str_radix(part, 16)
            .map_err(|_| "图片 AES key 含非法十六进制字符".to_string())?;
    }
    Ok(key)
}

fn aes128_ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if data.len() % 16 != 0 {
        return Err(format!(
            "图片密文长度不是 16 的倍数({} 字节)",
            data.len()
        ));
    }
    let cipher = Aes128::new_from_slice(key)
        .map_err(|error| format!("初始化图片 AES 解密失败:{error}"))?;
    let mut out = data.to_vec();
    for chunk in out.chunks_mut(16) {
        cipher.decrypt_block(Block::<Aes128>::from_mut_slice(chunk));
    }
    Ok(out)
}

/// 按文件头识别 MIME;无法识别时按 JPEG 处理(微信图片默认转码)。
pub fn guess_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/jpeg"
    }
}

fn ensure_allowed_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("图片下载地址非法:{error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("图片下载地址只允许 http/https".to_string());
    }
    let Some(host) = parsed.host_str() else {
        return Err("图片下载地址缺少主机名".to_string());
    };
    if !CDN_ALLOWLIST.contains(&host) {
        return Err(format!("图片下载地址不在微信 CDN 白名单内:{host}"));
    }
    Ok(())
}

/// 对 query 参数做百分号编码(`+`、`/`、`=` 等必须编码)。
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncrypt, KeyInit};

    fn image_with_key(media: Option<super::super::protocol::MediaRef>, aeskey: Option<&str>) -> ImageItem {
        ImageItem {
            media,
            mid_size: None,
            aeskey: aeskey.map(str::to_string),
            url: None,
            thumb_url: None,
        }
    }

    #[test]
    fn url_prefers_encrypted_query_param_and_encodes_it() {
        let image = image_with_key(
            Some(super::super::protocol::MediaRef {
                encrypt_query_param: Some("ab+c/=".into()),
                aes_key: None,
                full_url: None,
            }),
            None,
        );
        let url = image_url(&image).unwrap();
        assert_eq!(
            url,
            "https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=ab%2Bc%2F%3D"
        );
    }

    #[test]
    fn rejects_foreign_full_url() {
        let image = image_with_key(
            Some(super::super::protocol::MediaRef {
                encrypt_query_param: None,
                aes_key: None,
                full_url: Some("https://evil.example.com/x".into()),
            }),
            None,
        );
        assert!(image_url(&image).is_err());
    }

    #[test]
    fn parses_both_aes_key_forms() {
        let raw = [7u8; 16];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(parse_aes_key(&encoded).unwrap(), raw);
        let hex = raw
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let encoded_hex = base64::engine::general_purpose::STANDARD.encode(hex.as_bytes());
        assert_eq!(parse_aes_key(&encoded_hex).unwrap(), raw);
        assert!(parse_aes_key("nope").is_err());
    }

    #[test]
    fn decrypts_ecb_encrypted_payload() {
        let key = [9u8; 16];
        let plaintext: Vec<u8> = (0..32).collect();
        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut ciphertext = plaintext.clone();
        for chunk in ciphertext.chunks_mut(16) {
            cipher.encrypt_block(Block::<Aes128>::from_mut_slice(chunk));
        }
        let image = image_with_key(None, Some(&key.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        assert_eq!(decrypt_image(&image, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn guesses_common_image_mimes() {
        assert_eq!(guess_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(guess_mime(b"\x89PNG\r\n\x1a\nxxxx"), "image/png");
        assert_eq!(guess_mime(b"GIF89a...."), "image/gif");
        assert_eq!(guess_mime(b"RIFF....WEBP"), "image/webp");
        assert_eq!(guess_mime(b"unknown"), "image/jpeg");
    }
}
