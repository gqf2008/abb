//! 附件元数据（#12）：一份附件的 kind/来源/文件名/mime/大小/本地路径/sha256，
//! 以及给 agent 的一行摘要文本。桥把摘要注入 prompt，agent 按本地路径读文件。
//!
//! **落盘与读盘都不在本模块**（2026-11-04 内置渠道退役后）：入站下载落盘由各渠道
//! server 自己做（契约 §4「ABB 不负责渠道协议细节」），出站读文件 + 体积闸在
//! `mcp_channel::send_attachment` 里（先 metadata 后 read + `reject_if_too_big`）。
//! 这里只剩**跨渠道共用**的元数据结构与命名/摘要工具。

use serde::{Deserialize, Serialize};

/// 附件大小上限（字节）：飞书/微信参考实现均限 100MB，这里统一 200MB 兜底防异常超大文件打爆磁盘。
pub const MAX_ATTACHMENT_BYTES: usize = 200 * 1024 * 1024;

/// 一个已保存附件的元数据（也是给 agent 的摘要来源）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// 一条待下载附件的引用（桥 → messenger 分发用）。
///
/// **只有 MCP 一种形态**（2026-11-04 内置渠道退役后）：三个平台变体
/// （`Feishu` 带 `file_key` / `Dingtalk` 带 `download_code` / `Wechat` 带
/// `WechatMedia`）随 `src/{feishu,dingtalk,wechat}.rs` 一起删掉了——那些字段
/// 是**渠道协议细节**，现在住在各自的 MCP server 里（契约 §4）。
///
/// 留着这个空枚举而不是把参数整个去掉，是为了让 `download_attachment` 的签名
/// 保持稳定：将来若某个渠道需要额外提示，加一个变体即可，调用方不用改。
#[derive(Debug, Clone)]
pub enum AttachmentDesc {
    /// MCP 渠道：**不带任何平台凭据**。
    ///
    /// 怎么下载是渠道 server 的事——它自己按消息 id 反查（飞书查消息 API、
    /// 钉钉查 download_code、微信查 media）。ABB 侧只需知道「这是第几个附件」。
    ///
    /// 消息 id 也不放这里：`Messenger::download_attachment` 的 `mid` 参数
    /// 已经给了，放两处只会出现「两处不一致时信哪个」的问题。
    Mcp,
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

/// 从文件名猜附件种类（image/audio/video/file）。deliver CLI --file 转发附件元数据用。
pub fn kind_from_name(file_name: &str) -> String {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "svg" | "ico" => "image",
        "mp3" | "wav" | "ogg" | "amr" | "silk" | "m4a" | "aac" | "flac" => "audio",
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => "video",
        _ => "file",
    }
    .to_string()
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
    fn kind_from_name_works() {
        assert_eq!(kind_from_name("a.PNG"), "image");
        assert_eq!(kind_from_name("photo.jpeg"), "image");
        assert_eq!(kind_from_name("voice.mp3"), "audio");
        assert_eq!(kind_from_name("clip.mp4"), "video");
        assert_eq!(kind_from_name("report.pdf"), "file");
        assert_eq!(kind_from_name("无后缀"), "file");
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
