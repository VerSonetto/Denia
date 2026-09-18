//! 交互式终端(PTY):控制台右侧「终端」面板的后端。
//!
//! # 为什么要有独立 crate
//!
//! 面板终端和 `bash` 工具是**两种东西**:
//! - `bash` 工具是**非交互**的:一条命令进、退出码与输出出,给模型看的;
//! - 面板终端是**交互式**的:用户敲键盘、看 TUI 重绘、`Ctrl+C` 打断、
//!   跑 `vim`/`top` 这类全屏程序。这要求真 PTY(Windows 上是 ConPTY),
//!   而不是管道 —— 管道没有 TTY,`ls` 不会带颜色,`top` 直接报错。
//!
//! 所以这里用 [`portable-pty`] 起真伪终端,而不是复用 [`denia_tools::shell`]。
//!
//! # 与 ZCode 的对应关系
//!
//! ZCode 用 Electron + node-pty 实现同一件事(`resources/app.asar.unpacked/
//! node_modules/node-pty`)。这里保持同样的**会话语义**:
//! - 每个终端是一个独立 PTY 进程 + 独立 id;
//! - 尺寸(`cols`/`rows`)由前端 `fit()` 算好后下发,**创建时就带上**,
//!   避免 PTY 先用 80×24 起步再纠正造成首屏重排闪烁;
//! - 输出流按字节原样转发(ANSI 转义序列交给前端 xterm 解释),
//!   服务端**不做任何终端仿真**;
//! - 进程退出上报退出码,前端据此写 `[进程已退出]` 或自动关标签。
//!
//! # 线程模型
//!
//! `portable-pty` 的读写是**阻塞** `std::io`。阻塞调用不能直接放在 tokio
//! 运行时上(会占住 worker 线程),所以:
//! - 读循环跑在专用 `std::thread`,读到一块就 `broadcast::send`;
//! - 写走 `std::sync::Mutex<Box<dyn Write + Send>>`,同样在 `spawn_blocking`
//!   里调用;
//! - 事件广播用 `tokio::sync::broadcast`,与浏览器面板同一套订阅模式。

pub mod manager;

pub use manager::{TerminalEvent, TerminalInfo, TerminalManager, TerminalSnapshot, base64_decode};
