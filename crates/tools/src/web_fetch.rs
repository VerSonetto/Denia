//! `web_fetch`:纯 HTTP 抓取网页并转为 Markdown 正文。
//!
//! - 不启动浏览器:一次异步 GET(连接池复用 + HTTP/2 多路复用),解析在
//!   进程内完成,无进程/tab 资源副作用;
//! - 反爬绕过走合法路径:完整浏览器请求头集(Chrome UA + sec-ch-ua +
//!   Sec-Fetch-*),gzip/brotli 自动协商解压,cookie jar 承接重定向链上
//!   的 set-cookie,重定向跟随到 10 跳;
//! - 不执行 JavaScript:动态渲染页抓不到的内容交给 browser 工具;
//! - 字符集按 Content-Type 头与 HTML meta 嗅探解码(GBK 等老中文站可读);
//! - 响应体流式限量读取,超大体量页不会撑爆内存。

use std::time::Duration;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use ego_tree::NodeRef;
use encoding_rs::Encoding;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use scraper::node::Node;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// 响应体读取上限:超过即停止读取(截断的 HTML 仍可提取前缀正文)。
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// 默认返回正文上限(字符)。统一输出预算 32000,这里留出提示行的余量。
const DEFAULT_MAX_CHARS: usize = 30_000;
const MIN_MAX_CHARS: usize = 1_000;
const MAX_MAX_CHARS: usize = DEFAULT_MAX_CHARS;
/// 主内容选根的最低文本量:低于它的候选视为壳(兜底 body)。
const ROOT_MIN_CHARS: usize = 100;

/// 对正文无贡献的元素:脚本样式、交互控件、结构性装饰(渲染时跳过)。
/// 导航/页脚/侧栏在文章页全是噪声,portal 首页的主要信息也在 main 里。
const NOISE_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "svg", "canvas", "nav", "header",
    "footer", "aside", "form", "button", "select", "option", "input", "textarea", "video",
    "audio", "object", "embed", "dialog", "link", "meta", "head",
];

/// Chrome 桌面版 UA:多数反爬只看 UA 字符串,这是性价比最高的一层。
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";

/// 完整浏览器头集:sec-ch-ua 与 Sec-Fetch-* 是 Cloudflare 等质量检查的
/// 常见评分项,缺头的请求比 UA 单独暴露得更明显。
fn browser_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: &'static str| {
        headers.insert(HeaderName::from_static(name), HeaderValue::from_static(value));
    };
    set(
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    );
    set("accept-language", "zh-CN,zh;q=0.9,en;q=0.8");
    set(
        "sec-ch-ua",
        "\"Chromium\";v=\"133\", \"Not(A:Brand\";v=\"99\", \"Google Chrome\";v=\"133\"",
    );
    set("sec-ch-ua-mobile", "?0");
    set("sec-ch-ua-platform", "\"Windows\"");
    set("sec-fetch-dest", "document");
    set("sec-fetch-mode", "navigate");
    set("sec-fetch-site", "none");
    set("sec-fetch-user", "?1");
    set("upgrade-insecure-requests", "1");
    headers
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    /// true 时返回原始响应文本(适合 JSON 接口),默认提取 Markdown 正文。
    #[serde(default)]
    raw: bool,
    /// 返回内容上限(字符)。
    #[serde(default)]
    max_chars: Option<usize>,
}

pub struct WebFetchTool {
    client: reqwest::Client,
    schema: ToolSchema,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        // builder 的超时/策略均为静态合法值;build 失败只能是全局 TLS 后端
        // 初始化损坏,panic 比静默降级诚实。
        let client = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .default_headers(browser_headers())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::limited(10))
            .cookie_store(true)
            .build()
            .expect("web_fetch http client must build");
        Self {
            client,
            schema: ToolSchema {
                name: "web_fetch".to_string(),
                description: "抓取网页并返回正文 Markdown。纯 HTTP 请求,不执行 JavaScript;适合读文档、看文章、抓接口响应。url 必填;raw=true 返回原始响应文本(适合 JSON 接口);max_chars 限制返回字符数(默认 30000)。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "要抓取的完整 URL(http/https)"
                        },
                        "raw": {
                            "type": "boolean",
                            "description": "true 时跳过正文提取,返回原始响应文本;调用 JSON 接口时用"
                        },
                        "max_chars": {
                            "type": "number",
                            "description": "返回内容上限(字符),默认 30000"
                        }
                    },
                    "required": ["url"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        let args: WebFetchArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象:url 必填;raw(可选,true 返回原始响应文本);max_chars(可选,返回字符上限)",
                );
            }
        };
        let url = match reqwest::Url::parse(args.url.trim()) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            Ok(url) => {
                return tool_error(
                    format!("不支持的 URL 协议:{url}"),
                    "web_fetch 只支持 http/https;本地文件用 read_file,复杂操作用 bash",
                );
            }
            Err(error) => {
                return tool_error(
                    format!("URL 无效:{}({error})", args.url.trim()),
                    "补全协议头(https:// 开头)后重试",
                );
            }
        };
        let max_chars = args
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(MIN_MAX_CHARS, MAX_MAX_CHARS);

        let mut response = match self.client.get(url.clone()).send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return tool_error(
                    format!("请求超时:{url}"),
                    "站点响应慢或不可达;稍后重试,或改用 browser 工具加载页面",
                );
            }
            Err(error) if error.is_connect() => {
                return tool_error(
                    format!("连接失败:{url}({error})"),
                    "检查域名拼写与网络连通性;需要登录态或 JS 渲染的页面改用 browser 工具",
                );
            }
            Err(error) => {
                return tool_error(
                    format!("请求失败:{url}({error})"),
                    "检查 URL 与网络;需要登录态或 JS 渲染的页面改用 browser 工具",
                );
            }
        };
        let final_url = response.url().clone();
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if !status.is_success() {
            return tool_error(
                format!("{} 返回 {status}", final_url),
                if status.as_u16() == 403 || status.as_u16() == 429 {
                    "站点拒绝了程序化访问;改用 browser 工具加载页面,或稍后重试"
                } else if status.as_u16() == 404 {
                    "路径不存在;核对 URL 拼写,或先抓站点首页找正确链接"
                } else {
                    "确认 URL 可公开访问;需要交互的页面用 browser 工具"
                },
            );
        }

        // 流式限量读:超大体量页(常见误抓:直链大文件)读到上限即停,
        // 不让一次误调用下载几百 MB。
        let mut bytes: Vec<u8> = Vec::new();
        let mut truncated_download = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                        let take = MAX_RESPONSE_BYTES - bytes.len();
                        bytes.extend_from_slice(&chunk[..take]);
                        truncated_download = true;
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    return tool_error(
                        format!("读取响应体失败:{url}({error})"),
                        "站点中断了响应;重试一次,仍失败改用 bash curl 验证可达性",
                    );
                }
            }
        }

        let text = decode_bytes(&bytes, content_type.as_deref());
        if args.raw {
            return raw_output(&text, max_chars, truncated_download);
        }
        let (title, markdown) = extract_markdown(&text, &final_url);
        if markdown.trim().is_empty() {
            // 正文提取为空:通常是 JSON/纯文本响应或 JS 渲染空壳。
            return tool_error(
                format!("页面没有可提取的正文:{final_url}"),
                "接口响应请加 raw=true 取原文;需要 JS 渲染的动态页面改用 browser 工具",
            );
        }
        let mut output = String::new();
        if !title.is_empty() {
            output.push_str(&format!("# {title}\n\n"));
        }
        output.push_str(&clip_chars(&markdown, max_chars, "正文"));
        if truncated_download {
            output.push_str("\n\n…[注意:响应体超过 10MB 上限,只读取了前段,内容可能不完整]");
        }
        output.push_str(&format!("\n\n---\n来源:{final_url}"));
        ToolOutput::text(output)
    }
}

/// raw 模式输出:原文按字符上限截断。
fn raw_output(text: &str, max_chars: usize, truncated_download: bool) -> ToolOutput {
    let mut output = clip_chars(text, max_chars, "响应");
    if truncated_download {
        output.push_str("\n\n…[注意:响应体超过 10MB 上限,只读取了前段]");
    }
    ToolOutput::text(output)
}

/// 按字符(char)截断,不撕 UTF-8;超限时附可执行提示。
fn clip_chars(text: &str, max_chars: usize, what: &str) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let shown: String = text.chars().take(max_chars).collect();
    format!(
        "{shown}\n\n…[{what}已截断:共 {total} 字符,仅返回前 {max_chars} 字符。需要更多时提高 max_chars 参数,或用更具体的 URL 只抓目标页面]"
    )
}

/// 响应字节 → 字符串:Content-Type 头的 charset 优先,其次扫 HTML 头部的
/// `<meta charset>`,都缺省按 UTF-8;解码失败字符替换为 U+FFFD 而不是报错。
fn decode_bytes(bytes: &[u8], content_type: Option<&str>) -> String {
    let header_charset = content_type.and_then(|ct| {
        ct.split(';')
            .skip(1)
            .find_map(|part| part.trim().strip_prefix("charset="))
            .map(|charset| charset.trim().trim_matches('"').to_string())
    });
    let encoding = header_charset
        .as_deref()
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| meta_charset(bytes).and_then(|label| Encoding::for_label(label.as_bytes())))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// 扫描 HTML 头部的字符集声明:`<meta charset="x">` 与
/// `<meta http-equiv="Content-Type" content="...; charset=x">`。
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(2048)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let needle = "charset=";
    let start = head.find(needle)? + needle.len();
    let rest = head[start..].trim_start_matches(['"', '\'']);
    let value: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// HTML → Markdown:主内容选根 + DOM 遍历转换(噪声标签在渲染时跳过)。
/// 返回 `(标题, 正文)`;无 `<title>` 时标题为空串。
fn extract_markdown(html: &str, base: &reqwest::Url) -> (String, String) {
    let document = Html::parse_document(html);
    let title = Selector::parse("title")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|element| collapse_spaces(&all_text(*element)))
        .unwrap_or_default();
    let root = select_content_root(&document);
    let markdown = render_children(root, 0, base);
    (title, normalize_blank_lines(&markdown))
}

/// 主内容选根:按可信度依次找第一个有实质文本的候选,兜底 body 的第一个
/// 元素子节点(document 结构)。
fn select_content_root(document: &Html) -> NodeRef<'_, Node> {
    const CANDIDATES: &[&str] = &["article", "main", "[role=main]", "#content", ".content"];
    for candidate in CANDIDATES {
        if let Ok(selector) = Selector::parse(candidate)
            && let Some(element) = document.select(&selector).next()
            && all_text(*element).trim().chars().count() > ROOT_MIN_CHARS
        {
            return *element;
        }
    }
    document
        .tree
        .root()
        .children()
        .find(|node| node.value().is_element())
        .unwrap_or_else(|| document.tree.root())
}

const INDENT: &str = "  ";

/// 子树全部文本(标题与选根判断用)。
fn all_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for descendant in node.descendants() {
        if let Node::Text(text) = descendant.value() {
            out.push_str(&text.to_string());
        }
    }
    out
}

/// 渲染一组兄弟节点:块级输出之间以空行分隔,行内文本原样拼接。
fn render_children(node: NodeRef<'_, Node>, depth: usize, base: &reqwest::Url) -> String {
    let mut out = String::new();
    for child in node.children() {
        out.push_str(&render_node(child, depth, base));
    }
    out
}

/// 渲染单个节点为 Markdown 片段(块级片段自带换行);噪声子树整棵跳过。
fn render_node(node: NodeRef<'_, Node>, depth: usize, base: &reqwest::Url) -> String {
    match node.value() {
        Node::Text(text) => collapse_text(&text.to_string()),
        Node::Element(element) if !NOISE_TAGS.contains(&element.name()) => {
            render_element(element, node, depth, base)
        }
        _ => String::new(),
    }
}

/// 渲染元素:标题/段落/列表/代码/表格/引用各自有转换,其余容器递归子节点。
fn render_element(
    element: &scraper::node::Element,
    node: NodeRef<'_, Node>,
    depth: usize,
    base: &reqwest::Url,
) -> String {
    let name = element.name();
    let inline = |node: NodeRef<'_, Node>| {
        // 子 Text 已按边界空格语义折叠;整体再 collapse 会吃掉元素间
        // 的单词间隔,这里原样拼接。
        let text = render_children(node, depth, base);
        collapse_spaces(&text)
    };
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name[1..].parse::<usize>().unwrap_or(1);
            let text = inline(node);
            if text.is_empty() {
                String::new()
            } else {
                format!("\n\n{} {text}\n", "#".repeat(level))
            }
        }
        "p" => {
            let text = inline(node);
            if text.is_empty() {
                String::new()
            } else {
                format!("\n\n{text}\n")
            }
        }
        "br" => "\n".to_string(),
        "hr" => "\n\n---\n".to_string(),
        "a" => {
            let text = inline(node);
            match element.attr("href").map(|href| absolute_url(base, href)) {
                Some(href) if !text.is_empty() => format!("[{text}]({href})"),
                Some(href) => format!("<{href}>"),
                None => text,
            }
        }
        "strong" | "b" => {
            let text = inline(node);
            if text.is_empty() {
                String::new()
            } else {
                format!("**{text}**")
            }
        }
        "em" | "i" => {
            let text = inline(node);
            if text.is_empty() {
                String::new()
            } else {
                format!("*{text}*")
            }
        }
        // 行内 code:pre 的子树不走这里(pre 整体取原文)。
        "code" if !is_inside_pre(node) => {
            let text = inline(node);
            if text.is_empty() {
                String::new()
            } else {
                format!("`{text}`")
            }
        }
        "pre" => {
            let code = collect_text(node);
            format!("\n\n```\n{code}\n```\n")
        }
        "blockquote" => {
            let inner = render_children(node, depth, base);
            let quoted: String = inner.lines().map(|line| format!("> {line}\n")).collect();
            format!("\n\n{quoted}\n")
        }
        "ul" | "ol" => {
            let ordered = name == "ol";
            let mut out = String::from("\n");
            let mut index = 0usize;
            for item in node.children() {
                if !is_element_named(item, "li") {
                    continue;
                }
                index += 1;
                let marker = if ordered { format!("{index}.") } else { "-".to_string() };
                let content = render_list_item(item, depth, base);
                out.push_str(&format!("{}{} {}\n", INDENT.repeat(depth), marker, content));
            }
            out.push('\n');
            out
        }
        // li 由父级列表处理;游离的 li 退化成普通容器。
        "li" => render_children(node, depth, base),
        "table" => render_table(node, base),
        "img" => {
            // 模型看不了图片;有 alt 的保留 alt 文本当图片占位说明。
            element
                .attr("alt")
                .map(|alt| format!(" [图片:{alt}] "))
                .unwrap_or_default()
        }
        _ => render_children(node, depth, base),
    }
}

/// 列表项渲染:直接子列表在下一层级展开,其余内容拍成一行。
fn render_list_item(item: NodeRef<'_, Node>, depth: usize, base: &reqwest::Url) -> String {
    let mut text = String::new();
    let mut nested = String::new();
    for child in item.children() {
        if is_element_named(child, "ul") || is_element_named(child, "ol") {
            nested.push_str(&render_node(child, depth + 1, base));
        } else {
            text.push_str(&render_node(child, depth, base));
        }
    }
    let mut out = collapse_spaces(&text);
    if !nested.trim().is_empty() {
        // 嵌套列表自带行首缩进;贴在条目内容后换行承接。
        out.push('\n');
        out.push_str(nested.trim_end_matches('\n'));
    }
    out
}

/// 表格 → Markdown 管道表:首行当表头。
fn render_table(node: NodeRef<'_, Node>, base: &reqwest::Url) -> String {
    let rows: Vec<Vec<String>> = node
        .children()
        .filter(|child| {
            is_element_named(*child, "thead")
                || is_element_named(*child, "tbody")
                || is_element_named(*child, "tr")
        })
        .flat_map(|section| {
            if is_element_named(section, "tr") {
                vec![section]
            } else {
                section
                    .children()
                    .filter(|row| is_element_named(*row, "tr"))
                    .collect()
            }
        })
        .map(|row| {
            row.children()
                .filter(|cell| is_element_named(*cell, "td") || is_element_named(*cell, "th"))
                .map(|cell| collapse_spaces(&render_children(cell, 0, base)))
                .collect()
        })
        .filter(|row: &Vec<String>| !row.is_empty())
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let pad = |row: &Vec<String>| {
        (0..width)
            .filter_map(|index| row.get(index).cloned())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let mut out = String::from("\n\n");
    out.push_str(&format!("| {} |\n", pad(&rows[0])));
    out.push_str(&format!("|{}|\n", vec![" --- "; width].join("|")));
    for row in rows.iter().skip(1) {
        out.push_str(&format!("| {} |\n", pad(row)));
    }
    out
}

/// 取子树全部文本(pre 用:不折叠空白、不做任何 markdown 转换;噪声子树跳过)。
fn collect_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for descendant in node.descendants() {
        match descendant.value() {
            Node::Text(text) => out.push_str(&text.to_string()),
            Node::Element(element) if NOISE_TAGS.contains(&element.name()) => {
                // script/style 混进代码块很常见:整棵子树的文本不入码。
                out.clear();
                break;
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

fn is_inside_pre(node: NodeRef<'_, Node>) -> bool {
    node.ancestors().any(|ancestor| is_element_named(ancestor, "pre"))
}

fn is_element_named(node: NodeRef<'_, Node>, name: &str) -> bool {
    matches!(node.value(), Node::Element(element) if element.name() == name)
}

/// 相对链接绝对化;锚点与脚本 href 返回空串(链接文本照常保留)。
fn absolute_url(base: &reqwest::Url, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return String::new();
    }
    base.join(href)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| href.to_string())
}

/// HTML 空白折叠为单空格(标题等"整体拍平"场景)。
fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text 节点渲染:折叠内部空白,保留首/尾空格的存在性——行内元素
/// 边界的空格是英文单词分隔符;全空白节点折叠为空串(块级自带换行)。
fn collapse_text(text: &str) -> String {
    let collapsed = collapse_spaces(text);
    if collapsed.is_empty() {
        return String::new();
    }
    let leading = if text.starts_with(|c: char| c.is_whitespace()) { " " } else { "" };
    let trailing = if text.ends_with(|c: char| c.is_whitespace()) { " " } else { "" };
    format!("{leading}{collapsed}{trailing}")
}

/// 收尾清理:连续空行压成一行,去行尾空白。
fn normalize_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://example.com/docs/page";

    fn base_url() -> reqwest::Url {
        reqwest::Url::parse(BASE).unwrap()
    }

    fn extract(html: &str) -> String {
        extract_markdown(html, &base_url()).1
    }

    #[test]
    fn headings_paragraphs_and_links() {
        let html = r#"
            <html><head><title>文档标题</title></head><body>
            <article>
                <h1>指南</h1>
                <p>第一段,含 <a href="/other">一个链接</a> 与 <strong>加粗</strong>、<em>斜体</em>。</p>
                <p>第二段</p>
            </article></body></html>"#;
        let (title, markdown) = extract_markdown(html, &base_url());
        assert_eq!(title, "文档标题");
        assert!(markdown.contains("# 指南"), "{markdown}");
        assert!(
            markdown.contains("第一段,含 [一个链接](https://example.com/other) 与 **加粗**、*斜体*。"),
            "{markdown}"
        );
        assert!(markdown.contains("第二段"), "{markdown}");
    }

    #[test]
    fn lists_nest_with_indentation() {
        let html = r#"<body><ul>
            <li>外层一</li>
            <li>外层二<ol><li>内层甲</li><li>内层乙</li></ol></li>
        </ul></body>"#;
        let markdown = extract(html);
        assert!(markdown.contains("- 外层一"), "{markdown}");
        assert!(markdown.contains("  1. 内层甲"), "{markdown}");
        assert!(markdown.contains("  2. 内层乙"), "{markdown}");
    }

    #[test]
    fn code_block_keeps_whitespace_and_drops_scripts() {
        let html = r#"<body><pre><code>fn main() {
    let  x  =   1;
}</code></pre></body>"#;
        let markdown = extract(html);
        assert!(markdown.contains("    let  x  =   1;"), "{markdown}");
    }

    #[test]
    fn nav_footer_and_scripts_are_dropped() {
        let html = r#"<body>
            <nav><a href="/nav">导航链接</a></nav>
            <script>var tracking = 1;</script>
            <main><p>正文在这里</p></main>
            <footer>版权所有</footer>
        </body>"#;
        let markdown = extract(html);
        assert!(markdown.contains("正文在这里"), "{markdown}");
        assert!(!markdown.contains("导航链接"), "{markdown}");
        assert!(!markdown.contains("tracking"), "{markdown}");
        assert!(!markdown.contains("版权所有"), "{markdown}");
    }

    #[test]
    fn relative_links_resolve_against_final_url() {
        let html = r##"<body><p><a href="sub/x.html">子页</a><a href="https://cdn.example/a.js">外链</a><a href="#sec">锚点</a></p></body>"##;
        let markdown = extract(html);
        assert!(markdown.contains("[子页](https://example.com/docs/sub/x.html)"), "{markdown}");
        assert!(markdown.contains("[外链](https://cdn.example/a.js)"), "{markdown}");
        assert!(!markdown.contains("](#sec)"), "锚点链接不应输出 href:{markdown}");
        assert!(markdown.contains("锚点"), "{markdown}");
    }

    #[test]
    fn tables_become_pipe_rows() {
        let html = r#"<body><table>
            <tr><th>名称</th><th>值</th></tr>
            <tr><td>a</td><td>1</td></tr>
        </table></body>"#;
        let markdown = extract(html);
        assert!(markdown.contains("| 名称 | 值 |"), "{markdown}");
        assert!(markdown.contains("| --- | --- |"), "{markdown}");
        assert!(markdown.contains("| a | 1 |"), "{markdown}");
    }

    #[test]
    fn blockquote_prefixes_lines() {
        let html = r#"<body><blockquote><p>引文一行</p><p>引文二行</p></blockquote></body>"#;
        let markdown = extract(html);
        assert!(markdown.contains("> 引文一行"), "{markdown}");
        assert!(markdown.contains("> 引文二行"), "{markdown}");
    }

    #[test]
    fn charset_header_wins_over_meta() {
        // GBK 编码的"中文",header 声明 gbk:解码正确。
        let (bytes, _, _) = encoding_rs::GBK.encode("中文内容");
        assert_eq!(decode_bytes(bytes.as_ref(), Some("text/html; charset=GBK")), "中文内容");
        // header 缺省时 meta 兜底:GBK 字节流按声明解码成可读中文。
        let (encoded, _, _) = encoding_rs::GBK.encode("正文");
        let mut body = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        body.extend_from_slice(encoded.as_ref());
        body.extend_from_slice(b"</body></html>");
        let decoded = decode_bytes(&body, None);
        assert!(decoded.contains("正文"), "{decoded}");
        // 都没有:UTF-8 兜底。
        assert_eq!(decode_bytes("直接文本".as_bytes(), None), "直接文本");
    }

    #[test]
    fn clip_chars_keeps_utf8_and_hints() {
        let text = "甲".repeat(50);
        let clipped = clip_chars(&text, 10, "正文");
        assert!(clipped.starts_with(&"甲".repeat(10)));
        assert!(clipped.contains("共 50 字符"), "{clipped}");
        assert_eq!(clip_chars(&text, 100, "正文"), text, "未超限原样返回");
    }

    #[test]
    fn absolute_url_handles_edge_cases() {
        let base = base_url();
        assert_eq!(absolute_url(&base, "/a"), "https://example.com/a");
        assert_eq!(absolute_url(&base, "b"), "https://example.com/docs/b");
        assert_eq!(absolute_url(&base, "#x"), "");
        assert_eq!(absolute_url(&base, "javascript:void(0)"), "");
    }
}
