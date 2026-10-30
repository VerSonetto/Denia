//! Session indexing, paging and lifecycle.
use super::*;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy)]
struct PagePosition {
    index: u64,
    offset: u64,
}

struct PageScan {
    reader: BufReader<File>,
    header: SessionHeader,
    start: Option<PagePosition>,
    end_offset: u64,
    total: u64,
    anchors: Vec<SessionAnchor>,
    totals: SessionTotals,
}

fn scan_page(file: File, before: Option<u64>, limit: usize) -> Result<PageScan, SessionError> {
    let limit = limit.clamp(1, 1000);
    let eligible = |seq: u64| before.is_none_or(|cut| seq < cut);
    let mut reader = BufReader::new(file);
    let mut header = None;
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut offset = 0u64;
    let mut end_offset = 0u64;
    let mut total = 0u64;
    let mut anchors = Vec::new();
    let mut anchor_at: Vec<PagePosition> = Vec::new();
    let mut tail = VecDeque::with_capacity(limit);
    let mut totals = SessionTotals::default();
    let mut open_turn = None;

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line_start = offset;
        offset += read as u64;
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 与 next_log_event 一样,只有物理首行能作为 header;任意事件解码
        // 失败(含未知 type、嵌套字段损坏)即截止,不拼接其后的合法行。
        if line_no == 1 {
            header = Some(
                serde_json::from_str(trimmed)
                    .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?,
            );
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<SessionEnvelope>(trimmed) else {
            break;
        };
        totals.fold(&envelope, &mut open_turn);
        if is_transient_event(&envelope.event) {
            continue;
        }
        let position = PagePosition {
            index: total,
            offset: line_start,
        };
        if let SessionEvent::UserMessage {
            text,
            injected: false,
            ..
        } = &envelope.event
        {
            anchors.push(SessionAnchor {
                seq: envelope.seq,
                text: anchor_preview(text),
            });
            if eligible(envelope.seq) {
                anchor_at.push(position);
            }
        }
        if eligible(envelope.seq) {
            if tail.len() == limit {
                tail.pop_front();
            }
            tail.push_back(position);
            total += 1;
            end_offset = offset;
        }
    }
    let header = header.ok_or_else(|| SessionError::Corrupt("missing header".into()))?;
    let boundary = total.saturating_sub(limit as u64);
    let start = match anchor_at.binary_search_by(|position| position.index.cmp(&boundary)) {
        Ok(pos) => Some(anchor_at[pos]),
        Err(0) => tail.front().copied(),
        Err(insert) => Some(anchor_at[insert - 1]),
    };
    Ok(PageScan {
        reader,
        header,
        start,
        end_offset,
        total,
        anchors,
        totals,
    })
}

fn read_page_window(scan: PageScan, before: Option<u64>) -> Result<SessionPage, SessionError> {
    let PageScan {
        reader,
        header,
        start,
        end_offset,
        total,
        anchors,
        totals,
    } = scan;
    let mut events = Vec::new();
    if let Some(start) = start {
        let mut file = reader.into_inner();
        file.seek(SeekFrom::Start(start.offset))?;
        // 固定在第一遍最后一条 eligible 展示行的行尾。既不重读 before
        // 之后的尾部,也不让两遍之间的新 append 超出 total/anchors/totals。
        let mut reader = BufReader::new(file.take(end_offset - start.offset));
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(envelope) = serde_json::from_str::<SessionEnvelope>(trimmed) else {
                break;
            };
            if !is_transient_event(&envelope.event) && before.is_none_or(|cut| envelope.seq < cut) {
                events.push(envelope);
            }
        }
    }
    Ok(SessionPage {
        header,
        events,
        total,
        has_more_before: start.is_some_and(|position| position.index > 0),
        anchors,
        totals,
    })
}

/// 分页索引缓存条目数上限(LRU)。
///
/// 单条目只存展示事件的 `(行首偏移, seq)`,不存正文;打开过的长会话
/// 翻页/重连都命中同一份索引,不会重复全量解析。
const PAGE_INDEX_MAX_ENTRIES: usize = 4;

/// 单条目可索引的展示事件上限:超出就不驻留(退回每次全量扫描)。
///
/// 60k 条 × 16 字节 ≈ 1 MB,四条合计约 4 MB —— 换来的是长会话每次打开
/// 从 O(日志字节) 的全量 JSON 解析降到 O(新增字节) 的增量扫描。
const PAGE_INDEX_MAX_DISPLAY_EVENTS: usize = 60_000;

/// 日志超过该体积就不建索引:极端日志的展示事件表可能很大,退回
/// 每次全量扫描(语义不变,只是不吃这块内存)。
const PAGE_INDEX_MAX_LOG_BYTES: u64 = 128 * 1024 * 1024;

/// 一个会话的分页扫描增量状态。
///
/// 日志只追加,所以索引只需要"记住扫到哪、看到过什么":新请求从
/// [`PageIndex::scanned`] 继续扫新增行。文件变短(回退截断、损坏修复)
/// 时整条作废重建——那种情况下前缀也不再可信。
struct PageIndex {
    header: SessionHeader,
    /// 已扫描到的字节位置,永远停在完整行末尾。
    scanned: u64,
    /// 展示事件数,等于 `positions.len()`。
    total: u64,
    /// 展示事件按下标平行:`(行首字节偏移, seq)`。
    positions: Vec<(u64, u64)>,
    /// 全会话非注入 user-message 锚点,以及与它平行的展示序号。
    anchors: Vec<(SessionAnchor, u64)>,
    totals: SessionTotals,
    /// 未闭合轮次的配对状态(`SessionTotals::fold` 的跨事件输入)。
    open_turn: Option<(u32, u64)>,
}

/// 一次分页查询的窗口方案(只有偏移,不含正文)。
struct PagePlan {
    header: SessionHeader,
    total: u64,
    anchors: Vec<SessionAnchor>,
    totals: SessionTotals,
    /// 窗口起点;`None` 表示没有任何可读展示事件。
    start: Option<u64>,
    /// 窗口终点(含):扫描时确定的文件位置,不把本次请求期间新追加的
    /// 尾部混进旧的 total/anchors/totals 口径。
    end: u64,
    has_more_before: bool,
}

/// 分页索引缓存:按会话 id 存 `PageIndex`,LRU 淘汰。
pub(crate) struct PageIndexCache {
    entries: std::collections::HashMap<String, PageIndex>,
    order: VecDeque<String>,
}

impl PageIndexCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn take(&mut self, id: &str) -> Option<PageIndex> {
        self.order.retain(|entry| entry != id);
        self.entries.remove(id)
    }

    fn remove(&mut self, id: &str) {
        self.order.retain(|entry| entry != id);
        self.entries.remove(id);
    }

    fn put(&mut self, id: String, index: PageIndex) {
        self.order.retain(|entry| entry != &id);
        self.entries.insert(id.clone(), index);
        self.order.push_back(id);
        while self.order.len() > PAGE_INDEX_MAX_ENTRIES {
            if let Some(stale) = self.order.pop_front() {
                self.entries.remove(&stale);
            }
        }
    }

    /// 会话被删除时立刻回收,不让索引比日志多活一轮 LRU。
    pub(crate) fn forget(&mut self, id: &str) {
        self.remove(id);
    }
}

/// 从头建索引:与 [`scan_page`] 同样的首行规则(物理首行才是 header,
/// 空行跳过)、同样的"首个未知/损坏行处截止"。
fn build_page_index(file: &Path) -> Result<PageIndex, SessionError> {
    let mut reader = BufReader::new(File::open(file)?);
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut header = None;
    let mut index = PageIndex {
        header: SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id: String::new(),
            created_at: 0,
            cwd: String::new(),
            sandbox: false,
            parent_session: None,
            subagent: None,
        },
        scanned: 0,
        total: 0,
        positions: Vec::new(),
        anchors: Vec::new(),
        totals: SessionTotals::default(),
        open_turn: None,
    };
    let mut offset = 0u64;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        line_no += 1;
        let trimmed = line.trim();
        let line_end = offset + read as u64;
        if trimmed.is_empty() {
            offset = line_end;
            index.scanned = line_end;
            continue;
        }
        if line_no == 1 {
            let parsed: SessionHeader = serde_json::from_str(trimmed)
                .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?;
            header = Some(parsed);
            offset = line_end;
            index.scanned = line_end;
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<SessionEnvelope>(trimmed) else {
            break;
        };
        index.fold_line(&envelope, offset, line_end);
        offset = line_end;
    }
    let header = header.ok_or_else(|| SessionError::Corrupt("missing header".into()))?;
    index.header = header;
    Ok(index)
}

/// 从 `index.scanned` 续扫到文件当前末尾(含尾部残行:不解析成功就不前进)。
fn extend_page_index(file: &Path, index: &mut PageIndex) -> Result<(), SessionError> {
    let mut handle = File::open(file)?;
    handle.seek(SeekFrom::Start(index.scanned))?;
    let mut reader = BufReader::new(handle);
    let mut line = String::new();
    let mut offset = index.scanned;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        let line_end = offset + read as u64;
        if trimmed.is_empty() {
            offset = line_end;
            index.scanned = line_end;
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<SessionEnvelope>(trimmed) else {
            break;
        };
        index.fold_line(&envelope, offset, line_end);
        offset = line_end;
    }
    Ok(())
}

impl PageIndex {
    /// 折进一行已解析事件:统计、锚点、展示事件表与已扫描位置一起前进。
    fn fold_line(&mut self, envelope: &SessionEnvelope, line_start: u64, line_end: u64) {
        self.totals.fold(envelope, &mut self.open_turn);
        self.scanned = line_end;
        if is_transient_event(&envelope.event) {
            return;
        }
        if let SessionEvent::UserMessage {
            text,
            injected: false,
            ..
        } = &envelope.event
        {
            self.anchors.push((
                SessionAnchor {
                    seq: envelope.seq,
                    text: anchor_preview(text),
                },
                self.total,
            ));
        }
        self.positions.push((line_start, envelope.seq));
        self.total += 1;
    }

    /// 在一个 `before`/`limit` 查询下算出窗口与全会话统计。
    ///
    /// 与 [`scan_page`] 逐字段等价:展示粒度、锚点回退、`has_more_before`
    /// 都按"可入窗口的展示事件序号"计算,`before` 之外的展示事件只影响
    /// 锚点与统计,不进窗口。
    fn plan(&self, before: Option<u64>, limit: usize) -> PagePlan {
        let eligible = |seq: u64| before.is_none_or(|cut| seq < cut);
        let mut anchors = Vec::with_capacity(self.anchors.len());
        anchors.extend(self.anchors.iter().map(|(anchor, _)| anchor.clone()));

        // 一遍表扫描:窗口序号映射锚点(锚点与展示事件表平行推进)。
        let mut anchor_at: Vec<(u64, u64)> = Vec::new();
        let mut cursor = 0usize;
        let mut running = 0u64;
        for (index, (offset, seq)) in self.positions.iter().enumerate() {
            while cursor < self.anchors.len() && self.anchors[cursor].1 == index as u64 {
                if eligible(self.anchors[cursor].0.seq) {
                    anchor_at.push((running, *offset));
                }
                cursor += 1;
            }
            if eligible(*seq) {
                running += 1;
            }
        }
        let total = running;
        let boundary = total.saturating_sub(limit as u64);

        // 倒扫两项:最后一条可入窗口展示事件的终点,以及窗口起点的兜底偏移。
        let mut last_eligible = None;
        let mut need = total - boundary;
        let mut fallback = None;
        for (index, (offset, seq)) in self.positions.iter().enumerate().rev() {
            if !eligible(*seq) {
                continue;
            }
            last_eligible.get_or_insert(index);
            if need > 0 {
                need -= 1;
                if need == 0 {
                    fallback = Some(*offset);
                }
            }
        }
        let end = match last_eligible {
            Some(index) => self
                .positions
                .get(index + 1)
                .map_or(self.scanned, |(next, _)| *next),
            None => self.scanned,
        };

        let (start, has_more_before) =
            match anchor_at.binary_search_by(|(index, _)| index.cmp(&boundary)) {
                // 边界正好落在锚点:窗口从这条锚点开始。
                Ok(position) => (Some(anchor_at[position].1), anchor_at[position].0 > 0),
                // 边界在首条锚点之前:从边界那条展示事件开始。
                Err(0) => (fallback, boundary > 0 && fallback.is_some()),
                // 边界落在两锚点之间:回退到前一条锚点,保证窗口首轮次完整。
                Err(insert) => {
                    let anchor = anchor_at[insert - 1];
                    (Some(anchor.1), anchor.0 > 0)
                }
            };

        PagePlan {
            header: self.header.clone(),
            total,
            anchors,
            totals: self.totals,
            start,
            end,
            has_more_before,
        }
    }
}

/// 按字节区间读取窗口正文:与 [`read_page_window`] 同一套过滤(跳过
/// 流式增量、`before` 之外的行)与同样的"首个损坏行处截止"。
fn read_window_range(
    file: &Path,
    start: u64,
    end: u64,
    before: Option<u64>,
) -> Result<Vec<SessionEnvelope>, SessionError> {
    let mut handle = File::open(file)?;
    handle.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(handle.take(end.saturating_sub(start)));
    let mut events = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<SessionEnvelope>(trimmed) else {
            break;
        };
        if !is_transient_event(&envelope.event) && before.is_none_or(|cut| envelope.seq < cut) {
            events.push(envelope);
        }
    }
    Ok(events)
}

impl SessionStore {
    pub fn open(home: &Path) -> Result<Self, SessionError> {
        let root = home.join("sessions");
        std::fs::create_dir_all(&root)?;
        let store = Self {
            root,
            index: Mutex::new(std::collections::HashMap::new()),
            active: Mutex::new(std::collections::HashMap::new()),
            last_scan_ms: Mutex::new(0),
            page_indexes: Mutex::new(PageIndexCache::new()),
        };
        store.rescan_index()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 扫描目录重建摘要索引:每个文件只读 header + 首条用户消息(找到即停),
    /// 不读全文;文件损坏的会话静默跳过(与旧行为一致)。
    pub fn rescan_index(&self) -> Result<(), SessionError> {
        let mut fresh = std::collections::HashMap::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let file = entry.path().join("session.jsonl");
            if !file.is_file() {
                continue;
            }
            if let Some((meta, stamp)) = read_summary(&file) {
                fresh.insert(
                    meta.id.clone(),
                    IndexEntry {
                        meta,
                        stamp,
                        indexed_at: now_millis(),
                    },
                );
            }
        }
        *self.index.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        *self.last_scan_ms.lock().unwrap_or_else(|p| p.into_inner()) = now_millis();
        Ok(())
    }

    pub fn create(&self, cwd: &Path, sandbox: bool) -> Result<Session, SessionError> {
        self.create_with_parent(cwd, sandbox, None)
    }

    /// 分支创建:新会话继承父会话的 cwd/sandbox,血缘写进 header,
    /// 并把源日志前缀(`source[..cut]`)作为种子回放。
    pub fn create_forked(
        &self,
        source_events: &[SessionEnvelope],
        cut: usize,
        cwd: &Path,
        sandbox: bool,
        parent_id: &str,
    ) -> Result<Session, SessionError> {
        let session = self.create_with_parent(cwd, sandbox, Some(parent_id.to_string()))?;
        session.seed_from(&source_events[..cut])?;
        Ok(session)
    }

    pub fn create_subagent(
        &self,
        cwd: &Path,
        sandbox: bool,
        parent_id: &str,
        descriptor: denia_core::session::SubagentDescriptor,
    ) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        let session = Session::create_described(
            &dir,
            id,
            cwd,
            sandbox,
            Some(parent_id.into()),
            Some(descriptor),
        )?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    pub(super) fn create_with_parent(
        &self,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
    ) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        let session = Session::create(&dir, id, cwd, sandbox, parent_session)?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    /// 会话是否存在:只看文件在不在,不做任何解析。
    ///
    /// 上传接口原先为这一句判断走完整 `load` —— 整份 JSONL 读入、逐行反序列化、
    /// torn-tail 修复,还会**写盘**(孤儿轮次闭合),代价与收益完全不成比例。
    pub fn exists(&self, id: &str) -> bool {
        self.file_for(id)
            .map(|file| file.is_file())
            .unwrap_or(false)
    }

    pub fn load(&self, id: &str) -> Result<Session, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let session = Session::load(&file)?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    /// 冷打开会话(仅浏览路径):聚合投影保留,事件不驻留内存。
    ///
    /// 与 [`Self::load`] 的差异只在事件驻留;索引账本同样刷新。
    pub fn open_cold(&self, id: &str) -> Result<Session, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let session = Session::open_cold(&file)?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    /// 只读日志 header(第一行),不解析事件。
    pub fn read_log_header(&self, id: &str) -> Result<SessionHeader, SessionError> {
        let file = self.file_for(id)?;
        let mut reader = BufReader::new(File::open(&file)?);
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(SessionError::Corrupt("empty session log".into()));
        }
        serde_json::from_str(line.trim_end_matches(['\n', '\r']))
            .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))
    }

    /// 流式遍历日志中的合法事件(跳过退役类型行,遇 torn tail 截止),
    /// 不驻留任何事件。闭包返回 `Err(io::Error)` 即中止(如响应流断开)。
    pub fn for_each_event(
        &self,
        id: &str,
        mut f: impl FnMut(SessionEnvelope) -> Result<(), std::io::Error>,
    ) -> Result<(), SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let mut reader = BufReader::new(File::open(&file)?);
        let mut header = None;
        let mut line = String::new();
        let mut line_no = 0usize;
        while let Some(envelope) =
            next_log_event(&mut reader, &mut header, &mut line, &mut line_no)?
        {
            f(envelope)?;
        }
        Ok(())
    }

    /// 冷路径 checkpoints:流式扫日志收集全部非注入用户消息。
    pub fn read_checkpoints(&self, id: &str) -> Result<Vec<SessionEnvelope>, SessionError> {
        let mut out: Vec<SessionEnvelope> = Vec::new();
        self.for_each_event(id, |envelope| {
            if matches!(
                &envelope.event,
                SessionEvent::UserMessage {
                    injected: false,
                    ..
                }
            ) {
                out.push(envelope);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// 直接按文件流式读取一个「展示粒度」事件窗口,不构造/驻留完整 `Session`。
    ///
    /// - `before = None`:取日志尾部最近 `limit` 条。
    /// - `before = Some(seq)`:取 `seq` 之前最近 `limit` 条(供前端向上翻页)。
    /// - 分页单位是展示事件:流式 `assistant-chunk` 不计入 `total` 也不进入
    ///   窗口——历史重建只依赖结算的 `assistant-message`(失败尝试不进派生
    ///   历史),否则 chunk 洪流会让窗口在两三个轮次内触底。
    /// - 窗口起点对齐轮次组:展示边界(total-limit)回退到其前最近一条非
    ///   注入 user-message——保证窗口内第一个轮次完整,前端折叠概览依赖
    ///   成对的 turn-start/turn-end;边界落在首条锚点之前时直接用边界。
    /// - `anchors` 恒为全会话非注入 user-message(不受 `before` 限制),供
    ///   前端轮次轴渲染全部刻度。
    /// - `totals` 恒为全会话累计统计(同样不受 `before`/`limit` 限制):窗口
    ///   里没有早期轮次,前端若只从窗口折叠,刷新/翻页后状态栏数字就会缩水。
    /// - 第一遍流式校验整份日志,仅保留锚点偏移和最近 `limit` 条展示事件
    ///   的偏移;第二遍 seek 到对齐后的窗口,不再从头解码全部历史。
    ///   内存 O(锚点数 + limit + 窗口),不缓存历史正文。
    /// - **增量索引**:第一遍的结果(展示事件行偏移、锚点、累计统计、已扫描
    ///   字节)按会话缓存在 [`PageIndexCache`] 里,日志只追加,所以后续
    ///   打开/翻页/重连只扫新增字节;文件变短(回退截断、损坏修复)时
    ///   整条重建。超过驻留预算的超大日志退回每次全量扫描。
    pub fn read_page(
        &self,
        id: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<SessionPage, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        match self.page_plan(&file, id, before, limit)? {
            Some(plan) => {
                let events = match plan.start {
                    Some(start) => read_window_range(&file, start, plan.end, before)?,
                    None => Vec::new(),
                };
                Ok(SessionPage {
                    header: plan.header,
                    events,
                    total: plan.total,
                    has_more_before: plan.has_more_before,
                    anchors: plan.anchors,
                    totals: plan.totals,
                })
            }
            // 极端日志:不驻留索引,退回每次全量扫描。
            None => {
                let scan = scan_page(File::open(&file)?, before, limit)?;
                read_page_window(scan, before)
            }
        }
    }

    /// 取出(必要时重建并续扫)分页索引并算窗口;超预算返回 `None`。
    ///
    /// 锁内做扫描:冷扫描会阻塞其它会话的分页请求,但换掉的是"每次翻页
    /// 都全量解析一遍日志";热态临界区只剩 stat + 比较 + 一次窗口区间计算。
    fn page_plan(
        &self,
        file: &Path,
        id: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Option<PagePlan>, SessionError> {
        let limit = limit.clamp(1, 1000);
        let size = std::fs::metadata(file)?.len();
        if size > PAGE_INDEX_MAX_LOG_BYTES {
            self.page_indexes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .forget(id);
            return Ok(None);
        }
        let mut cache = self.page_indexes.lock().unwrap_or_else(|p| p.into_inner());
        let mut index = match cache.take(id) {
            // 变短 = 回退截断/损坏修复:前缀不再可信,整条重建。
            Some(index) if size >= index.scanned => index,
            _ => build_page_index(file)?,
        };
        if size > index.scanned {
            extend_page_index(file, &mut index)?;
        }
        if index.positions.len() > PAGE_INDEX_MAX_DISPLAY_EVENTS {
            // 不驻留:本次仍然直接用这份内存索引出页,下次请求再扫。
            return Ok(Some(index.plan(before, limit)));
        }
        let plan = index.plan(before, limit);
        cache.put(id.to_string(), index);
        Ok(Some(plan))
    }

    /// 登记一个被 `Arc` 持有的会话:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。弱引用不阻止淘汰。
    pub fn track_session(&self, session: &std::sync::Arc<Session>) {
        let meta = meta_of(session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: FileStamp {
                    size: 0,
                    mtime_ms: 0,
                },
                indexed_at: now_millis(),
            },
        );
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session.id().to_string(), std::sync::Arc::downgrade(session));
    }

    /// List summaries from the memory index and live registry.
    ///
    /// - 活跃(已加载)会话:纯内存读摘要,不碰文件、不等落盘。
    /// - 其余条目:对每个文件做 stat 校验,文件变了才重读摘要;
    ///   目录与索引的差集(新建文件)会被发现并补进索引——
    ///   不做任何全文读取。
    pub fn list(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let mut summaries: Vec<SessionSummary> = Vec::new();

        // 1) 活跃会话内存优先(顺带清理失效弱引用)。
        {
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            let stale: Vec<String> = active
                .iter()
                .filter_map(|(id, weak)| match weak.upgrade() {
                    Some(session) => {
                        summaries.push(summary_of(&session));
                        None
                    }
                    None => Some(id.clone()),
                })
                .collect();
            for id in stale {
                active.remove(&id);
            }
        }

        // 2) 目录校验节流:进程内 create/load/delete 都会直接维护索引,
        //    列表高频刷新不需要每次都对全部文件做 stat;外部直接写文件时,
        //    最多延迟 2 秒被下次全量扫描发现。
        let last_scan_value = {
            let mut last = self.last_scan_ms.lock().unwrap_or_else(|p| p.into_inner());
            let now = now_millis();
            let value = *last;
            if now.saturating_sub(value) >= 2000 {
                *last = now;
                value
            } else {
                value
            }
        };
        let should_scan = now_millis().saturating_sub(last_scan_value) >= 2000;

        {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            if should_scan {
                let indexed: std::collections::HashSet<String> = index.keys().cloned().collect();
                let mut discovered = Vec::new();
                for entry in std::fs::read_dir(&self.root)? {
                    let entry = entry?;
                    let file = entry.path().join("session.jsonl");
                    if !file.is_file() {
                        continue;
                    }
                    let id = entry.file_name().to_string_lossy().to_string();
                    if indexed.contains(&id) {
                        continue;
                    }
                    if summaries.iter().any(|s| s.id == id) {
                        continue;
                    }
                    if let Some((meta, stamp)) = read_summary(&file) {
                        discovered.push(IndexEntry {
                            meta,
                            stamp,
                            indexed_at: now_millis(),
                        });
                    }
                }
                for entry in discovered {
                    index.insert(entry.meta.id.clone(), entry);
                }

                // 索引条目 stat 失效检测。
                let ids: Vec<String> = index
                    .keys()
                    .filter(|id| !summaries.iter().any(|s| &s.id == *id))
                    .cloned()
                    .collect();
                for id in ids {
                    let file = self.root.join(&id).join("session.jsonl");
                    let current = file_stamp(&file);
                    let entry = index.get(&id);
                    let dirty = match (entry, current) {
                        (Some(entry), Some(current)) => entry.stamp != current,
                        (_, None) => true,
                        (None, _) => false,
                    };
                    if dirty {
                        if let Some((meta, stamp)) = read_summary(&file) {
                            index.insert(
                                id,
                                IndexEntry {
                                    meta,
                                    stamp,
                                    indexed_at: now_millis(),
                                },
                            );
                        } else {
                            index.remove(&id);
                        }
                    }
                }
            } else {
                // 刚创建/加载的条目还没经过一次全量校验:单独 stat 一次,
                // 保证“创建后立刻 append 再刷新列表”也能看到最新摘要。
                let recent_ids: Vec<String> = index
                    .iter()
                    .filter(|(_, entry)| entry.indexed_at > last_scan_value)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in recent_ids {
                    let file = self.root.join(&id).join("session.jsonl");
                    let current = file_stamp(&file);
                    let dirty = match (index.get(&id), current) {
                        (Some(entry), Some(current)) => entry.stamp != current,
                        (_, None) => true,
                        (None, _) => false,
                    };
                    if dirty {
                        if let Some((meta, stamp)) = read_summary(&file) {
                            index.insert(
                                id,
                                IndexEntry {
                                    meta,
                                    stamp,
                                    indexed_at: now_millis(),
                                },
                            );
                        } else {
                            index.remove(&id);
                        }
                    }
                }
            }
            // 活跃会话已在步骤 1 产出摘要,索引并入时跳过,避免同一会话出现两行。
            let listed: std::collections::HashSet<String> =
                summaries.iter().map(|s| s.id.clone()).collect();
            summaries.extend(
                index
                    .values()
                    .filter(|entry| !listed.contains(&entry.meta.id))
                    .map(|entry| summary_of_meta(&entry.meta)),
            );
        }

        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.created_at));
        Ok(summaries)
    }

    pub fn delete(&self, id: &str) -> Result<(), SessionError> {
        let dir = self.root.join(validate_id(id)?);
        if !dir.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        std::fs::remove_dir_all(dir)?;
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.page_indexes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .forget(id);
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        Ok(())
    }

    pub(super) fn file_for(&self, id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.root.join(validate_id(id)?).join("session.jsonl"))
    }

    /// 一次性清扫"从未发过消息的残留空白会话"(浏览器直接关闭、进程被杀
    /// 留下的空壳)。只在**进程启动时**调用一次,不在运行期反复执行。
    ///
    /// 清理职责属于数据层,不属于某个视图:前端每个页面只知道自己的焦点
    /// (`activeId`),却能看到全局会话列表;在那里做"非活跃空白即删"会误删
    /// 别的页面(或共用同一数据目录的另一实例)刚创建、正在使用的会话。
    ///
    /// `older_than_ms` 是给并发实例留的缓冲:只删足够老的空白,避免删掉
    /// 另一个实例里用户正停着的草稿。单个删除失败(Windows 上文件被占用)
    /// 只跳过,不让一次清理挡住启动。
    pub fn sweep_stale_blanks(&self, older_than_ms: u64) -> Vec<String> {
        let cutoff = now_millis().saturating_sub(older_than_ms);
        let candidates: Vec<String> = self
            .list()
            .unwrap_or_default()
            .into_iter()
            // 有内容的、分支血缘的(fork 子会话/委派)都保留,只动纯空白壳。
            .filter(|summary| {
                summary.excerpt.is_none()
                    && summary.parent_session.is_none()
                    && summary.created_at < cutoff
            })
            .map(|summary| summary.id)
            .collect();
        candidates
            .into_iter()
            .filter(|id| self.delete(id).is_ok())
            .collect()
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn seek_window_stops_at_metadata_scan_cutoff_when_events_are_appended() {
        let home = std::env::temp_dir().join(format!("denia-page-cutoff-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open(&home).unwrap();
        let session = store.create(&home, true).unwrap();
        let message = |text: &str| SessionEvent::UserMessage {
            text: text.into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        };
        session.append(message("before scan")).unwrap();
        session.flush().unwrap();
        let scan = scan_page(File::open(session.file()).unwrap(), None, 1).unwrap();
        session.append(message("after scan")).unwrap();
        session.flush().unwrap();
        let page = read_page_window(scan, None).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.anchors.len(), 1);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].seq, 1);
        let fresh = store.read_page(session.id(), None, 1).unwrap();
        assert_eq!(fresh.total, 2);
        assert_eq!(fresh.events[0].seq, 2);
        drop(session);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn seek_window_does_not_rejoin_a_tail_repaired_after_metadata_scan() {
        let home = std::env::temp_dir().join(format!("denia-page-repair-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open(&home).unwrap();
        let session = store.create(&home, true).unwrap();
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session.flush().unwrap();
        let file = session.file().to_path_buf();
        let prefix_len = std::fs::metadata(&file).unwrap().len();
        drop(session);
        {
            let mut writer = OpenOptions::new().append(true).open(&file).unwrap();
            writeln!(writer, "{{torn tail").unwrap();
        }
        let scan = scan_page(File::open(&file).unwrap(), None, 1).unwrap();
        {
            let mut writer = OpenOptions::new().write(true).open(&file).unwrap();
            writer.set_len(prefix_len).unwrap();
            writer.seek(SeekFrom::Start(prefix_len)).unwrap();
            serde_json::to_writer(
                &mut writer,
                &SessionEnvelope {
                    seq: 2,
                    time: 2,
                    event: SessionEvent::TurnEnd {
                        turn: 1,
                        reason: TurnEndReason::Completed,
                    },
                },
            )
            .unwrap();
            writeln!(writer).unwrap();
        }
        let page = read_page_window(scan, None).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.totals.turns, 0);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// 增量索引:打开后追加只扫新增字节,结果必须与全量扫描逐字段一致;
    /// 尾部残行不前进也不报错;文件变短(回退)后整条重建。
    #[test]
    fn page_index_extends_on_append_and_rebuilds_after_shrink() {
        let home = std::env::temp_dir().join(format!("denia-page-index-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open(&home).unwrap();
        let session = store.create(&home, true).unwrap();
        let id = session.id().to_string();
        let message = |text: &str| SessionEvent::UserMessage {
            text: text.into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        };
        let turn = |turn: u32| {
            [
                message(&format!("问题 {turn}")),
                SessionEvent::TurnStart { turn },
                SessionEvent::TurnEnd {
                    turn,
                    reason: TurnEndReason::Completed,
                },
            ]
        };
        for number in 1..=3u32 {
            for event in turn(number) {
                session.append(event).unwrap();
            }
        }
        session.flush().unwrap();
        // 每轮 3 条展示事件(用户消息 / turn-start / turn-end)。
        let first = store.read_page(&id, None, 2).unwrap();
        assert_eq!(first.total, 9);
        assert_eq!(first.totals.turns, 3);

        // 追加之后:续扫只能看新增字节(旧字节不再解析),统计跟着长。
        for number in 4..=5u32 {
            for event in turn(number) {
                session.append(event).unwrap();
            }
        }
        session.flush().unwrap();
        let extended = store.read_page(&id, None, 2).unwrap();
        assert_eq!(extended.total, 15);
        assert_eq!(extended.totals.turns, 5);
        assert_eq!(extended.anchors.len(), 5);
        let legacy = read_page_window(
            scan_page(File::open(session.file()).unwrap(), None, 2).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(legacy.total, extended.total);
        assert_eq!(legacy.events, extended.events);
        assert_eq!(legacy.totals, extended.totals);
        assert_eq!(legacy.anchors.len(), extended.anchors.len());

        // 尾部残行(写到一半):不解析成功就不前进,不报错也不改变统计。
        {
            let mut writer = OpenOptions::new()
                .append(true)
                .open(session.file())
                .unwrap();
            writer.write_all(b"{\"seq\":9").unwrap();
        }
        let torn = store.read_page(&id, None, 2).unwrap();
        assert_eq!(torn.total, 15);
        assert_eq!(torn.totals.turns, 5);

        // 回退截断到更早的字节位置:前缀不再可信,整条重建。
        let file = session.file().to_path_buf();
        let keep = {
            let bytes = std::fs::read(&file).unwrap();
            let mut offset = 0usize;
            let mut lines = 0usize;
            for (index, byte) in bytes.iter().enumerate() {
                if *byte != b'\n' {
                    continue;
                }
                lines += 1;
                if lines == 7 {
                    offset = index + 1;
                    break;
                }
            }
            offset
        };
        assert!(keep > 0);
        drop(session);
        {
            let writer = OpenOptions::new().write(true).open(&file).unwrap();
            writer.set_len(keep as u64).unwrap();
        }
        let rewound = store.read_page(&id, None, 2).unwrap();
        assert_eq!(rewound.total, 6);
        assert_eq!(rewound.totals.turns, 2);
        assert_eq!(rewound.anchors.len(), 2);

        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }
}
