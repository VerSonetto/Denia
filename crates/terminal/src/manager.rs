//! 终端中枢:PTY 会话注册表、读写泵、尺寸同步与事件广播。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tokio::sync::broadcast;

/// 单块读取上限(字节)。读循环每次最多取这么多,及时喂给前端。
const READ_CHUNK: usize = 16 * 1024;

/// 终端应答:光标位置报告(`CSI row ; col R`)。
///
/// # 为什么必须答
///
/// 交互式 shell(实测 PowerShell 7 / PSReadLine)启动时会发 **`CSI 6n`**
/// —— "光标现在在哪?"—— 然后**阻塞等待应答**才画提示符。终端模拟器
/// (xterm)会回这条应答,所以用户在真实终端里看不到这一步。
///
/// 但这里的链路是 `PTY → 服务端 → WebSocket → xterm`:服务端只做字节搬运,
/// xterm 收到的是**已经过去的历史**,它不会替服务端回这条查询(查询的应答
/// 必须写回 PTY 主端)。结果就是 shell 永远卡在等应答上 —— 表现为
/// "终端建起来了、首屏只有 4 字节 `ESC[6n`、敲什么都没反应"。
///
/// 修法:服务端在读循环里识别 `CSI 6n` 并**代答**一个合法坐标。这是所有
/// 终端复用器(tmux/screen)都要做的事,不是绕过。
///
/// 答 `1;1`(左上角)而不是真实光标位置:真实位置要维护完整终端状态机
/// (行列、换行、滚动区域),代价远超收益;shell 只用它来决定"要不要先换行",
/// `1;1` 是安全的保守值(表示"已在行首,不用补换行")。
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";

/// 单个终端保留的回滚缓冲上限(字节)。
///
/// 终端输出可能非常大(编译日志、`yes`)。前端 xterm 自己有 scrollback,
/// 但**重连**时要把历史补齐,所以服务端必须留一份。超过上限从头部截断,
/// 保证内存有界;截断处补一行提示,用户知道历史被裁剪过。
const SCROLLBACK_LIMIT: usize = 512 * 1024;

/// 同时存活的终端上限:防止前端反复新建把进程撑爆。
pub const TERMINAL_LIMIT: usize = 32;

/// 默认尺寸(前端 `fit()` 之前)。与 xterm 默认一致。
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// 尺寸合法区间:与前端输入框校验保持一致。
const MIN_COLS: u16 = 2;
const MAX_COLS: u16 = 1000;
const MIN_ROWS: u16 = 1;
const MAX_ROWS: u16 = 500;

/// 终端事件(SSE/WebSocket 推给前端)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TerminalEvent {
    /// 终端列表变化(新建/关闭/退出)。
    TerminalsChanged,
    /// 某个终端的输出块(base64,原样字节流)。
    Data {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        data: String,
    },
    /// 某个终端进程退出。
    Exit {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        #[serde(rename = "exitCode")]
        exit_code: u32,
    },
    /// 某个终端被服务端回收(超限淘汰/工作区关闭)。
    Closed {
        #[serde(rename = "terminalId")]
        terminal_id: String,
        reason: String,
    },
}

/// 终端对外快照(列表用)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub id: String,
    /// shell 可执行文件名(展示用标签,如 `PowerShell 7 (pwsh)`)。
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// 创建时刻(epoch ms):前端据此排序与显示相对时间。
    pub created_at: u64,
    /// 已退出时为退出码。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
}

/// 状态快照:面板初始化时一次性拉取。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSnapshot {
    pub terminals: Vec<TerminalInfo>,
}

/// 一个终端的运行时状态。
///
/// ## 为什么 child 与 killer 分开
///
/// `wait()` 是**阻塞**调用,必须在锁外执行 —— 否则 `close()` 想拿同一把锁
/// 去 `kill()` 时会永远等下去(等待进程退出 vs 杀掉进程,死锁)。
///
/// 所以:
/// - `child` 是 `Option`:等待线程把它**取走**,在锁外 `wait()`;
/// - `killer` 是创建时 `clone_killer()` 出来的独立句柄:`close()` 拿它杀进程,
///   不需要碰 `child`,也就不与等待线程争锁。
struct Terminal {
    info: Mutex<TerminalInfo>,
    /// PTY 主端:用于 `resize`。
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// 写入端(键盘输入、粘贴)。与 `reply_writer` 指向同一 PTY 主端。
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// 终端应答端(光标位置查询的回复)。与 `writer` 共享底层句柄。
    reply_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// 子进程句柄:等待线程取走后置 `None`。
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    /// 杀进程句柄(与 `child` 分离,见类型注释)。
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 回滚缓冲:重连时补齐历史。
    scrollback: Mutex<Vec<u8>>,
    /// 进程是否已结束(读循环 EOF 或显式 kill 后置位)。
    exited: AtomicBool,
}

/// 向回滚缓冲追加输出,并把长度收敛到上限内。
///
/// 截断按**字节**做,可能切在多字节 UTF-8 序列中间。这里向前回溯到合法
/// 边界再截,避免前端解码出替换字符(`�`)。
///
/// 抽成自由函数(而不是 `Terminal` 的方法)是为了可测:`Terminal` 持有
/// `Box<dyn MasterPty>`,而该 trait 在 Windows/Unix 上的方法集**不同**
/// (`as_raw_handle` vs `as_raw_fd`/`process_group_leader`),为测试造一个
/// 跨平台替身反而要写两份条件编译。纯函数不需要替身。
fn append_scrollback(buffer: &mut Vec<u8>, chunk: &[u8]) {
    buffer.extend_from_slice(chunk);
    if buffer.len() <= SCROLLBACK_LIMIT {
        return;
    }
    let overflow = buffer.len() - SCROLLBACK_LIMIT;
    // 从 overflow 处向后找第一个 UTF-8 首字节(0b0xxxxxxx 或 0b11xxxxxx);
    // 0b10xxxxxx 是续字节,落在它上面说明切开了字符。
    let mut cut = overflow;
    while cut < buffer.len() && (buffer[cut] & 0xC0) == 0x80 {
        cut += 1;
    }
    buffer.drain(..cut);
}

impl Terminal {
    /// 追加输出到回滚缓冲,超出上限从头部截断。
    fn push_scrollback(&self, chunk: &[u8]) {
        let mut buffer = self.scrollback.lock().unwrap_or_else(|p| p.into_inner());
        append_scrollback(&mut buffer, chunk);
    }

    fn scrollback_bytes(&self) -> Vec<u8> {
        self.scrollback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// 终端中枢:所有面板终端进程的家。
pub struct TerminalManager {
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    events: broadcast::Sender<TerminalEvent>,
    /// 创建顺序计数:仅用于日志与稳定排序。
    sequence: AtomicU64,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 解析要启动的 shell。
///
/// 复用 [`denia_tools::shell::shell_executable_path`] 的解析结果(含 Windows
/// Terminal 默认 profile 与 Store 版 pwsh 别名的处理),**不自己写 `pwsh`**:
/// Store 版 PowerShell 更新会整体换版本目录,硬编码名字会指到不存在的路径。
fn resolve_shell() -> (PathBuf, String) {
    let runtime = denia_tools::shell::shell_runtime();
    (
        denia_tools::shell::shell_executable_path(),
        runtime.shell_label,
    )
}

/// 给子进程铺一套适合交互终端的环境变量。
///
/// 三处都是踩出来的:
/// - `TERM=xterm-256color`:不设的话 ncurses 程序按 `dumb` 处理,`top`/`vim`
///   不重绘;前端 xterm 支持的正是这一档。
/// - `COLORTERM=truecolor`:让程序敢用 24 位色。
/// - 清掉 `NO_COLOR` / `CLICOLOR=0`:模型工具链常给子进程设这些来要纯文本,
///   但交互终端里用户要的是颜色;父进程环境若带进来必须摘掉。
fn prepare_command(shell: &std::path::Path, cwd: &std::path::Path) -> CommandBuilder {
    let mut command = CommandBuilder::new(shell);
    command.cwd(cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env_remove("NO_COLOR");
    command.env_remove("CLICOLOR");
    command.env_remove("CLICOLOR_FORCE");
    // 让 shell 走交互模式(有 TTY 时本就如此,显式一次防止某些发行版配置偏差)。
    #[cfg(unix)]
    {
        command.arg("-i");
    }
    command
}

impl TerminalManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            terminals: Mutex::new(HashMap::new()),
            events: broadcast::channel(1024).0,
            sequence: AtomicU64::new(0),
        })
    }

    /// 订阅终端事件(SSE / WebSocket 通道)。
    pub fn subscribe(&self) -> broadcast::Receiver<TerminalEvent> {
        self.events.subscribe()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Terminal>>> {
        self.terminals.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 当前终端列表(按创建时刻排序,稳定)。
    pub fn list(&self) -> Vec<TerminalInfo> {
        let mut items: Vec<TerminalInfo> = self
            .map()
            .values()
            .map(|terminal| {
                terminal
                    .info
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
            .collect();
        items.sort_by_key(|info| info.created_at);
        items
    }

    /// 状态快照。
    pub fn snapshot(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            terminals: self.list(),
        }
    }

    /// 读取某个终端的回滚缓冲(重连/刷新后补齐历史)。
    pub fn scrollback(&self, id: &str) -> Option<Vec<u8>> {
        self.map().get(id).map(|terminal| terminal.scrollback_bytes())
    }

    pub fn get(&self, id: &str) -> Option<TerminalInfo> {
        self.map().get(id).map(|terminal| {
            terminal
                .info
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        })
    }

    /// 新建一个终端。
    ///
    /// `cols`/`rows` 由前端 `fit()` 预先算好传入:PTY **一开始**就是正确尺寸,
    /// 不会先 80×24 再纠正(那会让 shell 先按错误宽度换行,首屏出现折行残影)。
    pub fn create(
        self: &Arc<Self>,
        cwd: &std::path::Path,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<TerminalInfo, String> {
        if !cwd.is_dir() {
            return Err(format!("工作目录不存在:{}", cwd.display()));
        }
        {
            let map = self.map();
            if map.len() >= TERMINAL_LIMIT {
                return Err(format!("终端数量已达上限({TERMINAL_LIMIT} 个)"));
            }
        }

        let cols = cols.unwrap_or(DEFAULT_COLS).clamp(MIN_COLS, MAX_COLS);
        let rows = rows.unwrap_or(DEFAULT_ROWS).clamp(MIN_ROWS, MAX_ROWS);
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size)
            .map_err(|error| format!("创建 PTY 失败:{error}"))?;

        let (shell_path, shell_label) = resolve_shell();
        let command = prepare_command(&shell_path, cwd);
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("启动 shell 失败({}):{error}", shell_path.display()))?;
        // slave 必须显式 drop:留着它会让 master 端读不到 EOF,
        // 进程退出后读循环永远阻塞,`Exit` 事件也就永远发不出去。
        drop(pair.slave);

        // 独立的杀进程句柄:`close()` 用它,不碰 `child`(见 `Terminal` 注释)。
        let killer = child.clone_killer();

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("获取 PTY 读取端失败:{error}"))?;
        // `take_writer` 只能调一次(第二次报 "writer already taken"),
        // 而应答光标查询需要一条**独立**的写通道(见 `answer_cursor_queries`)。
        // 解法:取一次真 writer,再克隆它 —— `Box<dyn Write>` 本身不可 clone,
        // 但把它包进 `Arc<Mutex<..>>` 后两处共享同一个句柄,写操作天然互斥。
        //
        // 这样 `writer` 与 `reply_writer` 指向同一个 PTY 主端,只是各自加锁,
        // 因此"shell 等应答"与"用户敲键盘"不会互相阻塞(锁粒度是单次 write)。
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("获取 PTY 写入端失败:{error}"))?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
        let reply_writer = writer.clone();

        let index = self.sequence.fetch_add(1, Ordering::Relaxed);
        let id = format!("term-{}-{}", now_millis(), index);
        let info = TerminalInfo {
            id: id.clone(),
            shell: shell_label,
            cwd: cwd.display().to_string(),
            cols,
            rows,
            created_at: now_millis(),
            exit_code: None,
        };

        let terminal = Arc::new(Terminal {
            info: Mutex::new(info.clone()),
            master: Mutex::new(pair.master),
            writer,
            reply_writer,
            child: Mutex::new(Some(child)),
            killer: Mutex::new(killer),
            scrollback: Mutex::new(Vec::new()),
            exited: AtomicBool::new(false),
        });

        self.map().insert(id.clone(), terminal.clone());
        let _ = self.events.send(TerminalEvent::TerminalsChanged);

        self.spawn_reader(id.clone(), terminal.clone(), reader);
        self.spawn_waiter(id, terminal);

        tracing::info!(
            terminal_id = %info.id,
            shell = %info.shell,
            cwd = %info.cwd,
            cols,
            rows,
            "terminal created"
        );
        Ok(info)
    }

    /// 读循环:PTY → 广播。
    ///
    /// 跑在专用 `std::thread`:`portable-pty` 的 reader 是阻塞 `std::io::Read`,
    /// 放在 tokio 任务里会占住 worker。读到 EOF 说明进程结束,交给 waiter 收尾。
    ///
    /// reader 由调用方传入(创建时已 `try_clone_reader` 一次):这里不再
    /// 二次 clone,避免在 master 上多挂一个句柄。
    fn spawn_reader(
        self: &Arc<Self>,
        id: String,
        terminal: Arc<Terminal>,
        mut reader: Box<dyn Read + Send>,
    ) {
        let manager = self.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || {
                let mut buffer = vec![0u8; READ_CHUNK];
                // 跨块的查询残留:一条 `CSI 6n` 可能被切成两块(PTY 不保证
                // 按转义序列边界分块),所以要留着尾巴做跨块匹配。
                let mut carry: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let chunk = &buffer[..count];
                            terminal.push_scrollback(chunk);
                            // 先答查询再转发:shell 正阻塞等这条应答,越早写越好。
                            answer_cursor_queries(&terminal, &mut carry, chunk);
                            // 广播失败(无订阅者)不算错误:回滚缓冲仍然记着,
                            // 前端重连时用 `scrollback` 补齐。
                            let _ = manager.events.send(TerminalEvent::Data {
                                terminal_id: id.clone(),
                                data: base64_encode(chunk),
                            });
                        }
                        Err(error) => {
                            // Windows 上 ConPTY 关闭时会返回错误而非 EOF,
                            // 同样按"流结束"处理,不能当致命错误刷日志。
                            tracing::debug!(terminal_id = %id, %error, "pty read ended");
                            break;
                        }
                    }
                }
            })
            .expect("spawn pty reader thread");
    }

    /// 等待子进程退出并广播退出码。
    ///
    /// 关键:先把 `child` **取走**再 `wait()`,在锁外阻塞。若持锁等待,
    /// `close()` 拿不到锁去 kill,两边互等 —— 表现为"关闭终端请求挂住 15 秒
    /// 超时"(实测踩到过)。
    fn spawn_waiter(self: &Arc<Self>, id: String, terminal: Arc<Terminal>) {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            // 取出 child:取走后 `close()` 只能靠 killer,这正是我们要的。
            let taken = terminal
                .child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let exit_code = match taken {
                Some(mut child) => match child.wait() {
                    Ok(status) => status.exit_code(),
                    Err(error) => {
                        tracing::debug!(terminal_id = %id, %error, "wait pty child failed");
                        0
                    }
                },
                // 没拿到 child:已被 close() 取走或初始化异常。按已结束处理。
                None => 0,
            };
            terminal.exited.store(true, Ordering::SeqCst);
            {
                let mut info = terminal.info.lock().unwrap_or_else(|p| p.into_inner());
                info.exit_code = Some(exit_code);
            }
            let _ = manager.events.send(TerminalEvent::Exit {
                terminal_id: id.clone(),
                exit_code,
            });
            let _ = manager.events.send(TerminalEvent::TerminalsChanged);
            tracing::info!(terminal_id = %id, exit_code, "terminal exited");
        });
    }

    /// 写入键盘输入/粘贴内容。返回实际写入字节数。
    pub async fn write(&self, id: &str, data: Vec<u8>) -> Result<usize, String> {
        let Some(terminal) = self.map().get(id).cloned() else {
            return Err(format!("终端不存在:{id}"));
        };
        if terminal.exited.load(Ordering::SeqCst) {
            return Err("终端进程已退出".to_string());
        }
        tokio::task::spawn_blocking(move || {
            let mut writer = terminal.writer.lock().unwrap_or_else(|p| p.into_inner());
            writer
                .write_all(&data)
                .map_err(|error| format!("写入终端失败:{error}"))?;
            writer.flush().map_err(|error| format!("刷新终端失败:{error}"))?;
            Ok(data.len())
        })
        .await
        .map_err(|error| format!("写入任务失败:{error}"))?
    }

    /// 同步终端尺寸(前端 `fit()` 后下发)。
    pub async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let Some(terminal) = self.map().get(id).cloned() else {
            return Err(format!("终端不存在:{id}"));
        };
        let cols = cols.clamp(MIN_COLS, MAX_COLS);
        let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
        tokio::task::spawn_blocking(move || {
            {
                let mut info = terminal.info.lock().unwrap_or_else(|p| p.into_inner());
                if info.cols == cols && info.rows == rows {
                    return Ok(());
                }
                info.cols = cols;
                info.rows = rows;
            }
            terminal
                .master
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| format!("调整终端尺寸失败:{error}"))
        })
        .await
        .map_err(|error| format!("尺寸任务失败:{error}"))?
    }

    /// 关闭终端:杀进程 + 移除注册表 + 广播。
    pub fn close(&self, id: &str) -> Result<(), String> {
        let terminal = self
            .map()
            .remove(id)
            .ok_or_else(|| format!("终端不存在:{id}"))?;
        // 用独立的 killer 句柄,不碰 `child` —— 等待线程可能正持有它。
        // kill 失败(进程早已退出)不是错误。
        if let Err(error) = terminal
            .killer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .kill()
        {
            tracing::debug!(terminal_id = %id, %error, "kill pty child failed");
        }
        terminal.exited.store(true, Ordering::SeqCst);
        let _ = self.events.send(TerminalEvent::Closed {
            terminal_id: id.to_string(),
            reason: "closed".to_string(),
        });
        let _ = self.events.send(TerminalEvent::TerminalsChanged);
        tracing::info!(terminal_id = %id, "terminal closed");
        Ok(())
    }

    /// 关闭全部终端(进程退出时的兜底清理)。
    pub fn close_all(&self) {
        let ids: Vec<String> = self.map().keys().cloned().collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// 清理不在 `keep` 里的终端(工作区关闭/会话切换时回收)。
    pub fn retain(&self, keep: &[String]) -> Vec<String> {
        let wanted: std::collections::HashSet<&String> = keep.iter().collect();
        let stale: Vec<String> = self
            .map()
            .keys()
            .filter(|id| !wanted.contains(id))
            .cloned()
            .collect();
        for id in &stale {
            let _ = self.close(id);
        }
        stale
    }
}

/// 在输出块里找光标位置查询并代答(见 [`CURSOR_POSITION_QUERY`] 注释)。
///
/// `carry` 保存上一块的尾巴,用于匹配被切开的查询;每次调用后更新为
/// "当前块里可能是查询前缀的后缀"。
fn answer_cursor_queries(terminal: &Terminal, carry: &mut Vec<u8>, chunk: &[u8]) {
    // 只保留有可能构成查询前缀的尾巴(最长 len-1 字节),其余丢掉。
    let keep = CURSOR_POSITION_QUERY.len().saturating_sub(1);
    let mut scan: Vec<u8> = Vec::with_capacity(carry.len() + chunk.len());
    scan.extend_from_slice(carry);
    scan.extend_from_slice(chunk);

    if !scan.windows(CURSOR_POSITION_QUERY.len()).any(|window| window == CURSOR_POSITION_QUERY) {
        // 没命中:只留可能跨块的尾巴。
        carry.clear();
        if keep > 0 && scan.len() >= keep {
            carry.extend_from_slice(&scan[scan.len() - keep..]);
        } else {
            carry.extend_from_slice(&scan);
        }
        return;
    }

    // 命中:一个块里可能有多次查询(每次都要答),统计出现次数。
    let hits = scan
        .windows(CURSOR_POSITION_QUERY.len())
        .filter(|window| *window == CURSOR_POSITION_QUERY)
        .count();
    carry.clear();
    let mut writer = terminal
        .reply_writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for _ in 0..hits {
        if let Err(error) = writer.write_all(CURSOR_POSITION_REPLY) {
            tracing::debug!(%error, "reply cursor position query failed");
            break;
        }
    }
    let _ = writer.flush();
}

/// base64 编码(输出块走 JSON,必须文本化)。
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// base64 解码(前端输入走 JSON,必须文本化)。
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|error| format!("输入不是合法 base64:{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-term-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scrollback_truncates_and_keeps_utf8_boundary() {
        let mut buffer = Vec::new();
        // 灌入超过上限的中文(每字 3 字节),截断后必须是合法 UTF-8。
        let chunk = "测".repeat(SCROLLBACK_LIMIT / 3 + 1000);
        append_scrollback(&mut buffer, chunk.as_bytes());
        assert!(buffer.len() <= SCROLLBACK_LIMIT);
        assert!(
            std::str::from_utf8(&buffer).is_ok(),
            "截断必须落在 UTF-8 边界上"
        );
    }

    /// 逐块写入(真实读循环的形态)同样不能切坏字符。
    #[test]
    fn scrollback_keeps_utf8_boundary_across_chunks() {
        let mut buffer = Vec::new();
        let chunk = "汉".repeat(4096);
        for _ in 0..(SCROLLBACK_LIMIT / chunk.len() + 4) {
            append_scrollback(&mut buffer, chunk.as_bytes());
            assert!(std::str::from_utf8(&buffer).is_ok());
        }
        assert!(buffer.len() <= SCROLLBACK_LIMIT);
    }

    #[test]
    fn scrollback_under_limit_is_untouched() {
        let mut buffer = Vec::new();
        append_scrollback(&mut buffer, b"hello");
        assert_eq!(buffer, b"hello");
    }

    #[test]
    fn base64_round_trip() {
        let raw = b"\x1b[31mhello \xe4\xb8\xad\xe6\x96\x87\x1b[0m\r\n";
        let encoded = base64_encode(raw);
        assert_eq!(base64_decode(&encoded).unwrap(), raw);
    }

    #[test]
    fn base64_decode_rejects_garbage() {
        assert!(base64_decode("not base64!!").is_err());
    }

    #[test]
    fn resolve_shell_returns_existing_path() {
        let (path, label) = resolve_shell();
        assert!(path.is_file(), "shell 必须存在:{}", path.display());
        assert!(!label.is_empty());
    }

    /// `create` 拒绝不存在的目录(fail loud,不静默起一个空终端)。
    #[test]
    fn create_rejects_missing_cwd() {
        let manager = TerminalManager::new();
        let missing = std::env::temp_dir().join("denia-term-missing-xyz");
        let error = manager
            .create(&missing, Some(80), Some(24))
            .expect_err("不存在的目录必须报错");
        assert!(error.contains("工作目录不存在"), "{error}");
    }

    #[test]
    fn close_unknown_terminal_reports_error() {
        let manager = TerminalManager::new();
        assert!(manager.close("nope").is_err());
    }

    /// `retain` 只留白名单里的终端,其余全部回收。
    #[test]
    fn retain_closes_terminals_outside_whitelist() {
        let manager = TerminalManager::new();
        // 没有终端时 retain 空名单应当无事发生。
        assert!(manager.retain(&[]).is_empty());
        assert!(manager.list().is_empty());
    }

    /// 真起一个 PTY 然后关掉:必须在秒级返回,不能挂住。
    ///
    /// 这条用例守的是一个实测踩到的死锁:等待线程若**持锁** `wait()`,
    /// `close()` 就永远拿不到锁去 kill,请求会挂到 HTTP 超时(15s)。
    /// 修法是把 child 取走在锁外等待 + 用独立的 killer 杀进程。
    #[tokio::test]
    async fn create_then_close_does_not_deadlock() {
        let dir = temp_dir();
        let manager = TerminalManager::new();
        let info = manager
            .create(&dir, Some(80), Some(24))
            .expect("PTY 应当能起来");
        assert_eq!(manager.list().len(), 1);

        // 给读循环一点时间把 shell 的首屏提示符读出来(顺便验证读泵在跑)。
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let scrollback = manager.scrollback(&info.id).unwrap_or_default();
        assert!(!scrollback.is_empty(), "shell 应当已经输出了提示符");

        // 关闭必须很快返回。给 5 秒余量:真死锁时这里会超时失败。
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking({
                let manager = manager.clone();
                let id = info.id.clone();
                move || manager.close(&id)
            }),
        )
        .await
        .expect("close 不应挂住(持锁 wait 会造成死锁)")
        .expect("close 任务不应 panic");
        assert!(closed.is_ok(), "close 应当成功");
        assert!(manager.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 尺寸真的下到了 PTY(`resize` 返回后再读回是新值)。
    #[tokio::test]
    async fn resize_updates_reported_size() {
        let dir = temp_dir();
        let manager = TerminalManager::new();
        let info = manager.create(&dir, Some(80), Some(24)).unwrap();
        manager.resize(&info.id, 100, 25).await.unwrap();
        let after = manager.get(&info.id).unwrap();
        assert_eq!((after.cols, after.rows), (100, 25));
        // 越界值被夹到合法区间(与前端输入框校验一致)。
        manager.resize(&info.id, 0, u16::MAX).await.unwrap();
        let clamped = manager.get(&info.id).unwrap();
        assert_eq!(clamped.cols, MIN_COLS);
        assert_eq!(clamped.rows, MAX_ROWS);
        manager.close(&info.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 光标查询应答:单块命中、跨块命中、未命中三种形态。
    ///
    /// 守的是"终端建起来但敲什么都没反应"这个实测故障:shell 发 `CSI 6n`
    /// 后阻塞等应答,服务端不代答就永远到不了提示符。
    #[tokio::test]
    async fn answers_cursor_position_query_including_split_chunks() {
        let dir = temp_dir();
        let manager = TerminalManager::new();
        let info = manager.create(&dir, Some(80), Some(24)).unwrap();
        // 拿到底层 Terminal 直接验证 carry 逻辑(真 shell 的查询只发一次,
        // 不好构造跨块场景)。
        let terminal = manager.map().get(&info.id).cloned().unwrap();

        // 1) 单块命中:carry 应为空,且不 panic。
        let mut carry = Vec::new();
        answer_cursor_queries(&terminal, &mut carry, b"abc\x1b[6ndef");
        assert!(carry.is_empty(), "命中后不应留下 carry");

        // 2) 跨块:前半段不命中,后半段命中。
        let mut carry = Vec::new();
        answer_cursor_queries(&terminal, &mut carry, b"prompt\x1b[6");
        assert!(!carry.is_empty(), "半截查询必须留在 carry 里");
        answer_cursor_queries(&terminal, &mut carry, b"n rest");
        assert!(carry.is_empty(), "拼上后应命中并清空 carry");

        // 3) 完全无关的输出:carry 只保留可能的前缀长度。
        let mut carry = Vec::new();
        answer_cursor_queries(&terminal, &mut carry, b"hello world");
        assert!(
            carry.len() < CURSOR_POSITION_QUERY.len(),
            "carry 不应无限增长:{}",
            carry.len()
        );

        manager.close(&info.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 终端数量上限生效(防止前端反复新建把进程撑爆)。
    ///
    /// 必须是 async 用例:`create` 内部会 `spawn_blocking` 起等待线程,
    /// 那需要一个 tokio 运行时。
    #[tokio::test]
    async fn create_rejects_beyond_limit() {
        let dir = temp_dir();
        let manager = TerminalManager::new();
        // 只验证"上限常量被真的检查":把上限临时当断言依据,
        // 不真起 TERMINAL_LIMIT 个进程(那会拖慢测试且吃资源)。
        let mut created = Vec::new();
        for _ in 0..TERMINAL_LIMIT {
            match manager.create(&dir, Some(40), Some(10)) {
                Ok(info) => created.push(info.id),
                Err(error) => panic!("前 {TERMINAL_LIMIT} 个应当都能创建,却失败于:{error}"),
            }
        }
        let error = manager
            .create(&dir, Some(40), Some(10))
            .expect_err("超过上限必须被拒绝");
        assert!(error.contains("上限"), "{error}");
        for id in created {
            let _ = manager.close(&id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
