//! Session indexing, paging and lifecycle.
use super::*;

impl SessionStore {
    pub fn open(home: &Path) -> Result<Self, SessionError> {
        let root = home.join("sessions");
        std::fs::create_dir_all(&root)?;
        let store = Self {
            root,
            index: Mutex::new(std::collections::HashMap::new()),
            active: Mutex::new(std::collections::HashMap::new()),
            last_scan_ms: Mutex::new(0),
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
    ///   窗口——历史重建只依赖结算的 `assistant-message`(dsh 语义:失败
    ///   尝试不进派生历史),否则 chunk 洪流会让窗口在两三个轮次内触底。
    /// - 窗口起点对齐轮次组:展示边界(total-limit)回退到其前最近一条非
    ///   注入 user-message——保证窗口内第一个轮次完整,前端折叠概览依赖
    ///   成对的 turn-start/turn-end;边界落在首条锚点之前时直接用边界。
    /// - `anchors` 恒为全会话非注入 user-message(不受 `before` 限制),供
    ///   前端轮次轴渲染全部刻度。
    /// - `totals` 恒为全会话累计统计(同样不受 `before`/`limit` 限制):窗口
    ///   里没有早期轮次,前端若只从窗口折叠,刷新/翻页后状态栏数字就会缩水。
    /// - 两遍顺序扫描:窗口起点依赖文件末尾才能确定的边界,单遍需无界缓存;
    ///   两遍只引入一次顺序 IO,内存 O(锚点数 + 窗口)。
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
        let limit = limit.clamp(1, 1000);
        let eligible = |seq: u64| before.is_none_or(|cut| seq < cut);

        // 第一遍:统计展示事件总数、收集全会话锚点与累计统计,并记录每个
        // eligible 锚点的展示位次(供起点回退二分)。
        let mut reader = BufReader::new(File::open(&file)?);
        let mut header: Option<SessionHeader> = None;
        let mut line = String::new();
        let mut line_no = 0usize;
        let mut total = 0u64;
        let mut anchors: Vec<SessionAnchor> = Vec::new();
        // (展示位次, seq):展示位次按 eligible 展示事件序号计。
        let mut anchor_at: Vec<(u64, u64)> = Vec::new();
        // 累计统计:吃整份日志(不受 `before`/`limit` 与 transient 过滤影响),
        // 前端刷新/翻页后状态栏的累计数字才不会缩水。
        let mut totals = SessionTotals::default();
        let mut open_turn: Option<(u32, u64)> = None;
        while let Some(envelope) =
            next_log_event(&mut reader, &mut header, &mut line, &mut line_no)?
        {
            totals.fold(&envelope, &mut open_turn);
            if is_transient_event(&envelope.event) {
                continue;
            }
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
                    anchor_at.push((total, envelope.seq));
                }
            }
            if eligible(envelope.seq) {
                total += 1;
            }
        }
        let header = header.ok_or_else(|| SessionError::Corrupt("missing header".into()))?;

        // 窗口起点:边界回退到其前最近锚点;无锚点可用则直接用边界。
        let boundary = total.saturating_sub(limit as u64);
        let start_index = match anchor_at.binary_search_by(|(index, _)| index.cmp(&boundary)) {
            Ok(pos) => anchor_at[pos].0,
            Err(0) => boundary,
            Err(insert) => anchor_at[insert - 1].0,
        };

        // 第二遍:收集 eligible 展示事件中位次 ≥ 起点的窗口。
        let mut reader = BufReader::new(File::open(&file)?);
        let mut header2: Option<SessionHeader> = None;
        let mut line_no = 0usize;
        let mut events: Vec<SessionEnvelope> = Vec::new();
        let mut index = 0u64;
        while let Some(envelope) =
            next_log_event(&mut reader, &mut header2, &mut line, &mut line_no)?
        {
            if is_transient_event(&envelope.event) {
                continue;
            }
            if !eligible(envelope.seq) {
                continue;
            }
            if index >= start_index {
                events.push(envelope);
            }
            index += 1;
        }
        Ok(SessionPage {
            header,
            events,
            total,
            has_more_before: start_index > 0,
            anchors,
            totals,
        })
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

        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
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
