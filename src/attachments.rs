//! 附件处理（过渡能力，#12）：入站非文本消息（图片/音视频/文件）下载后保存到
//! `workspaces/<bot_key>/attachments/YYYY-MM-DD/<mid>_<seq><ext>`，旁写 `meta.json`
//! 记录 mime/大小/sha256/来源/文件名；桥把元数据摘要文本注入 agent prompt，
//! agent 按本地路径读取文件内容（图片可描述、文件可分析）。
//!
//! 本模块只负责「落盘 + 元数据 + 摘要文本」；各平台下载在对应 client/messenger 里做。

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

/// 附件大小上限（字节）：飞书/微信参考实现均限 100MB，这里统一 200MB 兜底防异常超大文件打爆磁盘。
pub const MAX_ATTACHMENT_BYTES: usize = 200 * 1024 * 1024;

/// 一个已保存附件的元数据（也是给 agent 的摘要来源）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// 消息种类：image | audio | video | file。
    pub kind: String,
    /// 来源平台：feishu | wechat | dingtalk。
    pub source: String,
    /// 原始文件名（可能为空；落盘用安全化后的名字）。
    pub file_name: String,
    /// 猜测的 mime（飞书用响应 Content-Type；微信/钉钉按文件名后缀）。
    pub mime: String,
    /// 文件字节数。
    pub size: u64,
    /// 本地绝对路径（agent 可读）。
    pub path: String,
    /// 文件 sha256（hex）。
    pub sha256: String,
    /// 补充说明（语音转写文本 / 下载失败原因）。空 = 无。
    pub note: String,
}

impl AttachmentMeta {
    /// 拼成给 agent 的一行元数据文本（[图片] 来源=… 文件名=… mime=… 大小=… 本地路径=… sha256=…）。
    pub fn to_prompt_line(&self) -> String {
        let mut s = format!(
            "[{}] 来源={} 文件名={} mime={} 大小={} 本地路径={} sha256={}",
            self.kind, self.source, self.file_name, self.mime, self.size, self.path, self.sha256
        );
        if !self.note.is_empty() {
            s.push_str(&format!(" 备注={}", self.note));
        }
        s
    }
}

/// 一条待下载附件的平台引用（桥 → messenger 分发用）。
#[derive(Debug, Clone)]
pub enum AttachmentDesc {
    Feishu {
        message_id: String,
        file_key: String,
        /// image | file | audio | video
        kind: String,
        file_name: String,
    },
    Dingtalk {
        download_code: String,
        robot_code: String,
        /// image | file | audio | video
        kind: String,
        file_name: String,
        voice_text: String,
    },
    Wechat(crate::wechat::WechatMedia),
}

/// 下载/保存失败的占位元数据：路径/sha 为空，note 带原因。
/// 消息仍会交给 agent（agent 可如实告知用户附件下载失败），不静默丢消息。
pub fn failed_meta(
    kind: &str,
    source: &str,
    file_name: &str,
    err: &anyhow::Error,
) -> AttachmentMeta {
    AttachmentMeta {
        kind: kind.to_string(),
        source: source.to_string(),
        file_name: file_name.to_string(),
        mime: String::new(),
        size: 0,
        path: String::new(),
        sha256: String::new(),
        note: format!("下载失败: {err:#}"),
    }
}

/// 附件字节落盘 + 写 meta.json，返回元数据（bot 工作区根由调用方传入，便于测试）。
/// 目录按本地日期分：`attachments/YYYY-MM-DD/`；文件名 `<mid>_<seq><ext>`（mid 安全化）。
#[allow(clippy::too_many_arguments)]
pub fn save_attachment_in(
    workspace: &Path,
    mid: &str,
    seq: usize,
    kind: &str,
    source: &str,
    file_name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<AttachmentMeta> {
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        anyhow::bail!(
            "附件超过大小上限（{}MB），拒绝保存",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        );
    }
    let now = crate::schedule::DateTime::now();
    let dir = workspace
        .join("attachments")
        .join(format!("{:04}-{:02}-{:02}", now.year, now.month, now.day));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建附件目录失败: {}", dir.display()))?;

    let stem = format!("{}_{}", sanitize_mid(mid), seq);
    let path = dir.join(format!("{stem}{}", ext_from(file_name, mime, kind)));
    std::fs::write(&path, bytes).with_context(|| format!("写附件失败: {}", path.display()))?;

    let meta = AttachmentMeta {
        kind: kind.to_string(),
        source: source.to_string(),
        file_name: file_name.to_string(),
        mime: mime.to_string(),
        size: bytes.len() as u64,
        path: path.display().to_string(),
        sha256: sha256_hex(bytes),
        note: String::new(),
    };
    let meta_json = serde_json::to_string_pretty(&meta).context("附件 meta 序列化失败")?;
    crate::atomic_write_text(&path.with_extension("meta.json"), &meta_json)
        .with_context(|| format!("写附件 meta 失败: {}.meta.json", path.display()))?;
    Ok(meta)
}

/// 生产入口：保存到 `~/.agent-bridge/workspaces/<bot_key>/attachments/...`。
#[allow(clippy::too_many_arguments)]
pub fn save_attachment(
    bot_key: &str,
    mid: &str,
    seq: usize,
    kind: &str,
    source: &str,
    file_name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<AttachmentMeta> {
    save_attachment_in(
        &crate::workspace_dir(bot_key),
        mid,
        seq,
        kind,
        source,
        file_name,
        mime,
        bytes,
    )
}

/// mid 安全化：只保留字母数字与 `-_.`，其余换成 `_`（防止目录穿越/文件系统非法字符）。
fn sanitize_mid(mid: &str) -> String {
    if mid.is_empty() {
        return "unknown".to_string();
    }
    mid.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 从文件名/mime/种类推导落盘后缀。文件名后缀优先（安全字符才用），否则按 mime 映射，
/// 再退到种类兜底。
fn ext_from(file_name: &str, mime: &str, kind: &str) -> String {
    if let Some(ext) = file_name.rsplit('.').next() {
        let ext = ext.trim();
        if !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return format!(".{}", ext.to_ascii_lowercase());
        }
    }
    let m = mime.split(';').next().unwrap_or("").trim();
    let from_mime = match m {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/bmp" => ".bmp",
        "audio/mp3" => ".mp3",
        "audio/mpeg" => ".mp3",
        "audio/wav" => ".wav",
        "audio/x-wav" => ".wav",
        "audio/ogg" => ".ogg",
        "audio/amr" => ".amr",
        "audio/silk" => ".silk",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "application/x-tar" => ".tar",
        "text/plain" => ".txt",
        "application/json" => ".json",
        _ => "",
    };
    if !from_mime.is_empty() {
        return from_mime.to_string();
    }
    match kind {
        "image" => ".img".to_string(),
        "audio" => ".audio".to_string(),
        "video" => ".video".to_string(),
        _ => ".bin".to_string(),
    }
}

/// 按文件名后缀猜 mime（无后缀回落种类默认值）。微信/钉钉没有 Content-Type 时用。
pub fn mime_from_name(file_name: &str, kind: &str) -> String {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let by_ext = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "amr" => "audio/amr",
        "silk" => "audio/silk",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "exe" => "application/octet-stream",
        "dmg" => "application/octet-stream",
        "apk" => "application/vnd.android.package-archive",
        _ => "",
    };
    if !by_ext.is_empty() {
        return by_ext.to_string();
    }
    match kind {
        "image" => "image/*".to_string(),
        "audio" => "audio/*".to_string(),
        "video" => "video/*".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// SHA-256（hex）。附件完整性校验 + agent 引用标识。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 从文本里提取 URL（http/https，遇空白/引号/中文标点截断）。给 agent 的链接元数据用，不做抓取。
pub fn extract_urls(text: &str) -> Vec<String> {
    const TERMINATORS: &[char] = &[
        ' ', '\t', '\n', '\r', '<', '>', '"', '\'', '`', '（', '）', '，', '。', '；', '：', '、',
        '！', '？', '【', '】', '《', '》', '“', '”', '‘', '’', '…',
    ];
    let mut urls = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 识别 http:// 或 https://（大小写不敏感），返回 scheme 长度；不匹配返回 None
        let scheme_len = if matches!(chars[i], 'h' | 'H')
            && chars.get(i + 1).map(|c| c.eq_ignore_ascii_case(&'t')) == Some(true)
            && chars.get(i + 2).map(|c| c.eq_ignore_ascii_case(&'t')) == Some(true)
            && chars.get(i + 3).map(|c| c.eq_ignore_ascii_case(&'p')) == Some(true)
        {
            if chars.get(i + 4) == Some(&':')
                && chars.get(i + 5) == Some(&'/')
                && chars.get(i + 6) == Some(&'/')
            {
                Some(7)
            } else if chars.get(i + 4).map(|c| c.eq_ignore_ascii_case(&'s')) == Some(true)
                && chars.get(i + 5) == Some(&':')
                && chars.get(i + 6) == Some(&'/')
                && chars.get(i + 7) == Some(&'/')
            {
                Some(8)
            } else {
                None
            }
        } else {
            None
        };
        let Some(slen) = scheme_len else {
            i += 1;
            continue;
        };
        let mut j = i + slen;
        while j < chars.len() && !TERMINATORS.contains(&chars[j]) {
            j += 1;
        }
        let url: String = chars[i..j].iter().collect();
        // 去掉尾部常见英文标点，避免 URL 被句号/逗号截脏
        let url = url.trim_end_matches(['.', ',', ')', ']', '}']).to_string();
        if url.len() > slen {
            urls.push(url);
        }
        i = j;
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sanitize_mid_keeps_safe_chars() {
        assert_eq!(sanitize_mid("om_123-abc"), "om_123-abc");
        assert_eq!(sanitize_mid("a/b/c"), "a_b_c");
        assert_eq!(sanitize_mid("msg+X/=="), "msg_X___");
        assert_eq!(sanitize_mid(""), "unknown");
    }

    #[test]
    fn ext_from_name_prefers_safe_ext() {
        assert_eq!(ext_from("报告.PDF", "application/pdf", "file"), ".pdf");
        assert_eq!(ext_from("a.b.png", "image/png", "image"), ".png");
        // 危险/超长后缀回落 mime
        assert_eq!(ext_from("a.../../etc", "image/png", "image"), ".png");
        assert_eq!(
            ext_from("x.123456789012345678901", "image/png", "image"),
            ".png"
        );
        // 无 mime 映射回落 kind
        assert_eq!(ext_from("无后缀", "", "image"), ".img");
        assert_eq!(ext_from("", "", "file"), ".bin");
    }

    #[test]
    fn mime_from_name_works() {
        assert_eq!(mime_from_name("a.PNG", "image"), "image/png");
        assert_eq!(mime_from_name("a.mp4", "video"), "video/mp4");
        assert_eq!(mime_from_name("a.pdf", "file"), "application/pdf");
        assert_eq!(mime_from_name("无后缀", "audio"), "audio/*");
        assert_eq!(mime_from_name("", "file"), "application/octet-stream");
    }

    #[test]
    fn save_attachment_writes_file_and_meta() {
        let dir = std::env::temp_dir().join(format!("abb-att-test-{}", uuid::Uuid::new_v4()));
        let ws = dir.join("workspaces").join("bot-x");
        let bytes = b"hello attachment";
        let meta = save_attachment_in(
            &ws,
            "om_1/abc",
            0,
            "file",
            "feishu",
            "a.txt",
            "text/plain",
            bytes,
        )
        .expect("保存附件应成功");
        assert_eq!(meta.kind, "file");
        assert_eq!(meta.source, "feishu");
        assert_eq!(meta.mime, "text/plain");
        assert_eq!(meta.size, bytes.len() as u64);
        assert_eq!(meta.sha256, sha256_hex(bytes));
        assert!(meta.file_name == "a.txt");
        // 路径是绝对路径且落在附件日期目录
        let p = std::path::Path::new(&meta.path);
        assert!(p.is_absolute(), "附件路径应绝对: {}", meta.path);
        assert!(p.starts_with(ws.join("attachments")));
        // mid 安全化：om_1/abc → om_1_abc
        let fname = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(fname.starts_with("om_1_abc_0."), "文件名: {fname}");
        // meta.json 旁写
        let meta_path = p.with_extension("meta.json");
        let meta_text = std::fs::read_to_string(&meta_path).expect("meta.json 应存在");
        let parsed: serde_json::Value = serde_json::from_str(&meta_text).unwrap();
        assert_eq!(parsed["sha256"], meta.sha256);
        assert_eq!(parsed["size"], bytes.len() as u64);
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_urls_basic() {
        let urls = extract_urls("看看 https://example.com/a?b=1 和 http://x.cn 不错");
        assert_eq!(urls, vec!["https://example.com/a?b=1", "http://x.cn"]);
    }

    #[test]
    fn extract_urls_strips_chinese_punct_and_trailing_dot() {
        let urls = extract_urls("参考https://docs.rs/aes，以及https://example.com/end. 结束");
        assert_eq!(urls, vec!["https://docs.rs/aes", "https://example.com/end"]);
    }

    #[test]
    fn extract_urls_uppercase_scheme() {
        let urls = extract_urls("HTTPS://EXAMPLE.COM/x");
        assert_eq!(urls, vec!["HTTPS://EXAMPLE.COM/x"]);
    }

    #[test]
    fn extract_urls_none() {
        assert_eq!(extract_urls("没有链接"), Vec::<String>::new());
        assert_eq!(extract_urls(""), Vec::<String>::new());
    }

    #[test]
    fn attachment_prompt_line() {
        let meta = AttachmentMeta {
            kind: "image".into(),
            source: "feishu".into(),
            file_name: "a.png".into(),
            mime: "image/png".into(),
            size: 10,
            path: "/tmp/a.png".into(),
            sha256: "ab".into(),
            note: String::new(),
        };
        assert_eq!(
            meta.to_prompt_line(),
            "[image] 来源=feishu 文件名=a.png mime=image/png 大小=10 本地路径=/tmp/a.png sha256=ab"
        );
    }
}
