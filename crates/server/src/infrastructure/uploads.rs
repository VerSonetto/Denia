//! Upload paths, names and writes; shared by application commands and HTTP adapters.
const MAX_NAME_CHARS: usize = 120;

/// 把字节写入 `<home>/uploads/<session_id>/`,返回(消毒后的文件名, 绝对路径)。
///
/// 上传端点与粘贴图片落盘共用这一条:同一目录、同一套文件名消毒、同名追加
/// 数字后缀且绝不覆盖。`session_id` 须已由 [`sanitize_id`] 校验。
pub(crate) fn write_upload(
    home: &std::path::Path,
    session_id: &str,
    raw_name: &str,
    bytes: &[u8],
) -> Result<(String, std::path::PathBuf), String> {
    sanitize_id(session_id)?;
    let file_name = sanitize_name(raw_name);
    let target_dir = home.join("uploads").join(session_id);
    std::fs::create_dir_all(&target_dir).map_err(|error| format!("create upload dir: {error}"))?;

    // create_new atomically reserves a name even when uploads arrive concurrently.
    let mut counter = 0usize;
    let target = loop {
        let name = if counter == 0 {
            file_name.clone()
        } else {
            format!("{file_name}.{counter}")
        };
        let target = target_dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .map_err(|error| format!("write upload: {error}"))?;
                break target;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => counter += 1,
            Err(error) => return Err(format!("create upload: {error}")),
        }
    };
    Ok((file_name, target))
}

/// 删除会话的附件目录 `<home>/uploads/<session_id>/`。
///
/// 粘贴图片现在每次落盘一张(带 pasted-N.png 占位名),不随会话删除回收就是
/// 只增不减的磁盘占用;目录名即会话 id,会话日志已删则其中文件再无引用。
pub(crate) fn remove_session_uploads(
    home: &std::path::Path,
    session_id: &str,
) -> Result<(), String> {
    sanitize_id(session_id).map_err(|error| format!("skip uploads cleanup: {error}"))?;
    let dir = home.join("uploads").join(session_id);
    if !dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).map_err(|error| format!("remove {}: {error}", dir.display()))
}

/// 粘贴图片的落盘名:有原始文件名就用它(仍走消毒),否则合成
/// `pasted-<seq>.<ext>` —— 名字里的 `pasted-` 前缀是前端识别"占位名"的口径。
pub(crate) fn pasted_image_name(name: Option<&str>, mime: &str, index: usize) -> String {
    if let Some(raw) = name.filter(|value| !value.trim().is_empty()) {
        return raw.to_string();
    }
    let ext = mime
        .strip_prefix("image/")
        .map(|suffix| {
            suffix
                .split([';', '+'])
                .next()
                .unwrap_or("png")
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .filter(|suffix: &String| !suffix.is_empty())
        .unwrap_or_else(|| "png".to_string());
    format!("pasted-{}.{}", index + 1, ext)
}

/// 会话 id 即目录名,必须挡住 `../` 之类的穿越 —— 附件目录与 uploads 回收
/// 都以此为唯一边界。
pub(crate) fn sanitize_id(id: &str) -> Result<String, String> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if valid {
        Ok(id.to_string())
    } else {
        Err("invalid id".to_string())
    }
}

/// 取 basename、去路径分隔符与非法字符,防路径注入与目录穿越。
fn sanitize_name(name: &str) -> String {
    let base = std::path::Path::new(name.rsplit(['/', '\\']).next().unwrap_or(name))
        .file_name()
        .map(|part| part.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let cleaned: String = base
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'
            )
        })
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized_to_a_single_component() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a/b\\c.txt"), "c.txt");
        assert_eq!(sanitize_name(""), "upload.bin");
        assert_eq!(sanitize_name("weird:*?name.txt"), "weirdname.txt");
    }

    #[test]
    fn pasted_names_fall_back_to_indexed_stems() {
        // 名字里的 pasted- 前缀是前端识别"占位名、不展示"的口径。
        assert_eq!(pasted_image_name(None, "image/png", 0), "pasted-1.png");
        assert_eq!(
            pasted_image_name(Some("  "), "image/jpeg", 2),
            "pasted-3.jpeg"
        );
        assert_eq!(pasted_image_name(None, "image/svg+xml", 1), "pasted-2.svg");
        // 非 image/ 前缀的怪 MIME 回落到 png,且扩展名不得逃出字符集。
        assert_eq!(
            pasted_image_name(None, "application/octet-stream", 0),
            "pasted-1.png"
        );
        assert_eq!(
            pasted_image_name(None, "image/gif;base64", 0),
            "pasted-1.gif"
        );
        assert_eq!(
            pasted_image_name(Some("截图..(1).png"), "image/png", 0),
            "截图..(1).png"
        );
    }

    #[test]
    fn write_upload_creates_and_never_overwrites() {
        let home = std::env::temp_dir().join(format!("denia-upload-test-{}", std::process::id()));
        let sid = "11111111-1111-1111-1111-111111111111";
        let (_, first) = write_upload(&home, sid, "pasted-1.png", b"one").unwrap();
        let (_, second) = write_upload(&home, sid, "pasted-1.png", b"two").unwrap();
        assert_ne!(first, second, "同名必须追加后缀,不得覆盖");
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ids_are_validated() {
        assert!(sanitize_id("9b7c2a6e-2f2a-4c8a-8d3c-a0f4b2e7c1d2").is_ok());
        assert!(sanitize_id("../escape").is_err());
        assert!(sanitize_id("").is_err());
    }
}
