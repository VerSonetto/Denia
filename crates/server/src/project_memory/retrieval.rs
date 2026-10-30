//! 记忆检索:纯 Rust、零依赖、零模型调用的字面相关性排序。
//!
//! ## 为什么不是向量检索
//!
//! 记忆库的实际体量决定了这里不需要向量:一个工作区的索引通常几百字节到
//! 两 KB,条目文件几十个。向量检索解决的是"候选太多、语义不字面"的问题,
//! 而这里两个前提都不成立。代价却很实在:要一个 embedding 端点(用户得再
//! 配一个 key)、一个本地模型(安装包涨几十 MB、笔记本 CPU 常驻),或者一个
//! 外部服务(个人开发者的软件不该依赖它)。
//!
//! ## 打分口径
//!
//! 查询按**字符 bigram** 切开(中文天然是二字词,英文按字符对),与
//! `name` / `description` / 正文分别比对,取覆盖率而不是命中次数:
//!
//! - 覆盖率 = 命中的不同 bigram 数 / 查询的不同 bigram 数。这样长文档不会
//!   因为篇幅大而占便宜。
//! - 字段权重 `name 3.0 > description 2.0 > 正文 1.0`,因为名字和摘要才是
//!   作者对"这条记忆讲什么"的判断。
//! - 单字查询(长度 1)退化成字符包含,否则 bigram 为空集合、任何文档都是 0。
//!
//! 这不是语义检索:问"登录怎么做的"不会命中只写了"认证"的条目。这是刻意
//! 的取舍——它可解释、可预测、零成本,而记忆条目的 `description` 本来就是
//! 为了让下一次判断相关性而写的。

use std::collections::BTreeSet;
use std::path::Path;

use super::{MAX_FILE_BYTES, MemoryKind, MemoryStatus, read_frontmatter, scan_manifest};

/// 一条命中。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHit {
    /// 相对记忆目录的路径(`/` 分隔)。
    pub name: String,
    /// 0.0–1.0 的相关性;字段加权后截断到 1.0。
    pub score: f32,
    /// frontmatter 的一句话摘要。
    pub description: Option<String>,
    pub kind: Option<MemoryKind>,
    pub status: Option<MemoryStatus>,
    pub size: u64,
    pub modified_ms: u64,
    /// 命中所在的字段,便于设置页解释"为什么它排前面"。
    pub matched: Vec<String>,
    /// 正文摘录(命中位置周围的一段,已在 UTF-8 边界截断)。
    pub excerpt: String,
}

/// 检索结果。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchResult {
    pub query: String,
    pub hits: Vec<MemoryHit>,
    /// 参与比对的条目数(不含索引本身与超大文件)。
    pub scanned: usize,
    /// 被跳过的大文件数(超过 [`MAX_FILE_BYTES`])。
    pub skipped: usize,
}

/// 摘录的最大字符数。
const EXCERPT_CHARS: usize = 240;
/// 单次检索返回的最大命中数上限。
pub const MAX_HITS: usize = 50;

/// 在记忆目录里按字面相关性检索。
///
/// `kind` / `include_superseded` 用于设置页筛选;`None` 表示不筛。
/// 排序:分数降序 → mtime 降序 → 文件名升序(保证结果稳定)。
pub fn search(
    root: &Path,
    query: &str,
    limit: usize,
    kind: Option<MemoryKind>,
    include_superseded: bool,
) -> MemorySearchResult {
    let query = query.trim();
    let limit = limit.clamp(1, MAX_HITS);
    let needles = query_grams(query);
    let mut hits: Vec<MemoryHit> = Vec::new();
    let mut scanned = 0usize;
    let mut skipped = 0usize;

    for file in scan_manifest(root).files {
        if file.size > MAX_FILE_BYTES {
            skipped += 1;
            continue;
        }
        let absolute = root.join(&file.name);
        let Ok(body) = std::fs::read_to_string(&absolute) else {
            continue;
        };
        let front = read_frontmatter(&absolute);
        if let Some(wanted) = kind
            && front.kind != Some(wanted)
        {
            continue;
        }
        if !include_superseded && front.status == Some(MemoryStatus::Superseded) {
            continue;
        }
        scanned += 1;
        if needles.is_empty() {
            continue;
        }
        let scored = score_document(&needles, &file.name, front.description.as_deref(), &body);
        let Some((score, matched)) = scored else {
            continue;
        };
        hits.push(MemoryHit {
            name: file.name.clone(),
            score,
            description: front.description,
            kind: front.kind,
            status: front.status,
            size: file.size,
            modified_ms: file.modified_ms,
            matched,
            excerpt: excerpt(&body, &needles),
        });
    }

    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.modified_ms.cmp(&left.modified_ms))
            .then_with(|| left.name.cmp(&right.name))
    });
    hits.truncate(limit);

    MemorySearchResult {
        query: query.to_string(),
        hits,
        scanned,
        skipped,
    }
}

/// 查询切成去重后的 bigram 集合;长度 1 时退化成单字。
fn query_grams(query: &str) -> BTreeSet<String> {
    let chars: Vec<char> = query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    grams(&chars)
}

fn grams(chars: &[char]) -> BTreeSet<String> {
    if chars.is_empty() {
        return BTreeSet::new();
    }
    if chars.len() == 1 {
        return BTreeSet::from([chars[0].to_string()]);
    }
    chars
        .windows(2)
        .map(|pair| pair.iter().collect::<String>())
        .collect()
}

/// 文档切 gram 后的覆盖率。`lower` 已转小写,避免每字段重复分配。
fn coverage(needles: &BTreeSet<String>, haystack: &str) -> f32 {
    if needles.is_empty() || haystack.is_empty() {
        return 0.0;
    }
    let chars: Vec<char> = haystack.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    let grams = grams(&chars);
    if needles.len() == 1 {
        // 单字查询:直接看字符是否出现。
        let needle = needles.iter().next().map(String::as_str).unwrap_or("");
        let hit = haystack.to_lowercase().contains(needle);
        return if hit { 1.0 } else { 0.0 };
    }
    let hit = needles
        .iter()
        .filter(|needle| grams.contains(*needle))
        .count();
    hit as f32 / needles.len() as f32
}

/// 命中注释:只有覆盖率非 0 才算命中。
fn score_document(
    needles: &BTreeSet<String>,
    name: &str,
    description: Option<&str>,
    body: &str,
) -> Option<(f32, Vec<String>)> {
    let mut score = 0.0f32;
    let mut matched: Vec<String> = Vec::new();
    let name_score = coverage(needles, name);
    if name_score > 0.0 {
        score += name_score * 3.0;
        matched.push("name".to_string());
    }
    if let Some(description) = description {
        let description_score = coverage(needles, description);
        if description_score > 0.0 {
            score += description_score * 2.0;
            matched.push("description".to_string());
        }
    }
    let body_score = coverage(needles, body);
    if body_score > 0.0 {
        score += body_score;
        matched.push("body".to_string());
    }
    if matched.is_empty() {
        return None;
    }
    // 字段加权后可能超过 1.0(三处都满覆盖),归一到 0–1 便于展示与设阈值。
    let normalized = (score / 6.0).min(1.0);
    Some((normalized, matched))
}

/// 取命中最密的一段正文;`needles` 全未命中时退化为开头。
///
/// 逐字符滑窗统计窗口内的覆盖率,取最高分的窗口起点。窗口本身就短,
/// 直接线性扫足够,不需要先建索引。
fn excerpt(body: &str, needles: &BTreeSet<String>) -> String {
    let plain = strip_frontmatter(body);
    let chars: Vec<char> = plain.chars().collect();
    if chars.len() <= EXCERPT_CHARS {
        return plain.trim().to_string();
    }
    let mut best_start = 0usize;
    let mut best_score = 0.0f32;
    // 步长取窗口的 1/8,够找到"命中密集"的位置,又不必逐字扫。
    let step = (EXCERPT_CHARS / 8).max(1);
    let mut start = 0usize;
    while start + EXCERPT_CHARS <= chars.len() {
        let window: String = chars[start..start + EXCERPT_CHARS].iter().collect();
        let score = coverage(needles, &window);
        if score > best_score {
            best_score = score;
            best_start = start;
        }
        start += step;
    }
    let tail: String = chars[best_start..(best_start + EXCERPT_CHARS).min(chars.len())]
        .iter()
        .collect();
    // 窗口起点可能落在句子中间:去掉开头的半个词,读起来完整些。
    let trimmed = tail.trim_start();
    let lead = if best_start == 0 { "" } else { "…" };
    format!("{lead}{trimmed}…")
}

/// 去掉 YAML frontmatter,只留正文。
fn strip_frontmatter(body: &str) -> &str {
    let trimmed = body.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return trimmed;
    };
    // 找第二个独立成行的 `---`。
    let mut offset = 0usize;
    for line in rest.lines() {
        offset += line.len() + 1;
        if line.trim() == "---" {
            return rest.get(offset..).unwrap_or("").trim_start();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_memory::MEMORY_INDEX_FILE;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-ret-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, front: &str, body: &str) {
        std::fs::write(dir.join(name), format!("---\n{front}\n---\n\n{body}\n")).unwrap();
    }

    #[test]
    fn bigram_coverage_prefers_name_and_description() {
        let root = temp_root("rank");
        write(
            &root,
            "edit-tool.md",
            "name: edit-tool-single-edit\nkind: experience\ndescription: edit 工具多条 edits 数组会序列化失败",
            "实测 edits 数组多段会失败,单个对象一次成功。",
        );
        write(
            &root,
            "unrelated.md",
            "name: c-drive-cleanup\nkind: fact\ndescription: C 盘清理记录",
            "删了 40G 缓存,与 edit 无关的文字里也提到过一次 edit。",
        );
        let result = search(&root, "edit 工具 edits", 10, None, true);
        assert_eq!(result.hits[0].name, "edit-tool.md", "{:?}", result.hits);
        assert!(result.hits[0].score > 0.3, "{}", result.hits[0].score);
        assert!(result.hits[0].matched.contains(&"name".to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn superseded_and_kind_filters_apply() {
        let root = temp_root("filter");
        write(
            &root,
            "old.md",
            "name: old\nkind: fact\nstatus: superseded\ndescription: 旧状态",
            "旧",
        );
        write(
            &root,
            "new.md",
            "name: new\nkind: fact\nstatus: active\ndescription: 新状态",
            "新",
        );
        write(
            &root,
            "exp.md",
            "name: exp\nkind: experience\ndescription: 经验状态",
            "经验",
        );

        let active = search(&root, "状态", 10, None, false);
        assert!(
            active.hits.iter().all(|hit| hit.name != "old.md"),
            "默认不返回 superseded:{:?}",
            active.hits
        );
        let all = search(&root, "状态", 10, None, true);
        assert_eq!(all.hits.len(), 3);
        let facts = search(&root, "状态", 10, Some(MemoryKind::Fact), false);
        assert!(
            facts
                .hits
                .iter()
                .all(|hit| hit.kind == Some(MemoryKind::Fact))
        );
        assert_eq!(facts.hits.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn single_char_query_does_not_panic_and_matches() {
        let root = temp_root("single");
        write(&root, "a.md", "name: alpha\ndescription: 关于部署", "正文");
        let result = search(&root, "部", 10, None, true);
        assert_eq!(result.hits.len(), 1, "{:?}", result.hits);
        assert!(result.hits[0].score > 0.0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blank_query_returns_no_hits_but_reports_scan() {
        let root = temp_root("blank");
        write(&root, "a.md", "name: a\ndescription: x", "y");
        let result = search(&root, "   ", 10, None, true);
        assert!(result.hits.is_empty());
        assert_eq!(result.scanned, 1, "空查询也要报告扫了多少条");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn excerpt_strips_frontmatter_and_stays_bounded() {
        let body = format!("---\nname: a\n---\n\n{}", "很长的正文".repeat(200));
        let out = strip_frontmatter(&body);
        assert!(!out.contains("name: a"), "摘录不得带 frontmatter:{out}");
        let short = excerpt(&body, &query_grams("正文"));
        assert!(short.chars().count() <= EXCERPT_CHARS + 2);
    }

    #[test]
    fn index_file_is_not_a_candidate() {
        let root = temp_root("index");
        std::fs::write(root.join(MEMORY_INDEX_FILE), "- [a](a.md) — 部署\n").unwrap();
        write(&root, "a.md", "name: a\ndescription: 部署", "正文");
        let result = search(&root, "部署", 10, None, true);
        assert!(
            result.hits.iter().all(|hit| hit.name != MEMORY_INDEX_FILE),
            "索引本身不参与检索"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
