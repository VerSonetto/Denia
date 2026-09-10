//! 持久 shell 会话:同一会话的多次 `bash` 调用共享一个长驻 shell 进程。
//!
//! ## 为什么需要它
//!
//! 一次性 shell 把"状态"这件事推给了模型:下一句命令拿不到上一句的变量与
//! 工作目录,于是模型只有一个办法把多步逻辑跑完——**把整段程序压成一条内联
//! 命令**。实测(反编译逆向会话,36 条 bash)单条命令平均 564 字符、最长 943,
//! `$ErrorActionPreference` 这类样板出现在 34/36 条里,`Write-Host` 打点 9 条:
//! 那些脚手架全是模型为了在"一次机会"里拿到可观测性而自己撑出来的。
//!
//! 持久 shell 让 `cd`、变量、函数跨调用保留,模型就能分步试、分步看,不必赌。
//! 这是 dsh 的默认形态(`tool-bash-persistent`:state persists across calls),
//! denia 之前只抄了一次性的那个变体。
//!
//! ## 协议
//!
//! 命令包在一条**物理单行**里,两端套随机 nonce 标记(抄 dsh):
//!
//! - PowerShell:`Write-Output '<START>'; <命令>; Write-Output ('<END>' + $__s)`
//! - POSIX:`printf '%s\n' '<START>'; eval '<命令>'; printf '%s%s\n' '<END>' "$__s"`
//!
//! 读侧在累积输出里找 `<END>` 后的数字当退出码,取 START 与 END 之间为正文。
//! nonce 每次调用唯一,所以命令自身的输出里出现同名标记也不会误判。包装必须
//! 是物理单行:换行会让 REPL 把后续内容当续行,把提示符泄漏进结果。
//!
//! ## 形态差异
//!
//! PowerShell 的 `-Command` 会把 stdin 读干净再执行,没法当长驻 REPL,所以
//! 这里传一个「读一行 → Invoke-Expression」的循环脚本作**参数**,进程起来后
//! 用 stdin 逐条喂命令;`Invoke-Expression` 在当前作用域求值,状态自然保留。
//! POSIX 的 bash 本身就把管道 stdin 当逐行输入,直接裸起即可。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::shell;

/// shell 方言:决定包装语法与转义规则。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Posix,
    PowerShell,
}

impl Dialect {
    fn detect() -> Self {
        if shell::shell_runtime().dialect == "powershell" {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }
}

/// PowerShell 侧的长驻循环。三处都是踩出来的:
///
/// 1. `-Command` 模式下 `[Console]::In` 读不到重定向的 stdin,必须用
///    `[Console]::OpenStandardInput()` 显式打开;
/// 2. 标记由**循环本人生成**,于是命令输出与标记走 `[Console]::Out` 同一条
///    路径,顺序稳定;
/// 3. 脚本里**一个双引号都不能有**:它要作为 `-Command` 的 Windows 参数传下去,
///    Rust 会把参数整体包进双引号并把内部 `"` 转成 `\"`,而 PowerShell 的命令行
///    解析与 MSVCRT 规则不同,`\"` 会被解析坏,子进程起来就废。需要引号时一律
///    用 `[char]34` 现拼。换行也留在物理单行里(命令侧折成 `` `n ``)。
const PWSH_LOOP: &str = concat!(
    "$q = [string][char]34; ",
    "$reader = New-Object System.IO.StreamReader([Console]::OpenStandardInput()); ",
    "while ($true) { ",
    "$l = $reader.ReadLine(); if ($null -eq $l) { break }; ",
    "$i = $l.IndexOf([char]9); if ($i -lt 0) { continue }; ",
    "$n = $l.Substring(0, $i); $c = $l.Substring($i + 1); ",
    "$LASTEXITCODE = $null; $ok = $true; ",
    "try { $r = Invoke-Expression ($q + $c + $q) 2>&1 | Out-String; $ok = $? } ",
    "catch { $r = '[denia-shell] ' + $_.Exception.Message; $ok = $false }; ",
    "if ($null -ne $LASTEXITCODE) { $s = [int]$LASTEXITCODE } elseif ($ok) { $s = 0 } else { $s = 1 }; ",
    "[Console]::Out.WriteLine('__DENIA_SHELL_START_' + $n + '__'); ",
    "[Console]::Out.Write($r); ",
    "[Console]::Out.WriteLine('__DENIA_SHELL_END_' + $n + ':' + $s) }"
);

/// 输出缓冲上限:只保留尾部,防止长命令把内存吃光。
const SCROLLBACK_CAP: usize = 1 << 20;

/// 一次命令的捕获结果。
#[derive(Debug, Clone)]
pub struct Captured {
    /// 命令正文(START 与 END 之间),已去掉首尾换行。
    pub text: String,
    /// 退出码;POSIX 用 `$?`,PowerShell 优先 `$LASTEXITCODE` 否则 `$?` 折算。
    pub exit_code: Option<i32>,
}

struct Shared {
    bytes: Vec<u8>,
    closed: bool,
}

/// 常驻循环脚本的落盘路径(按进程 id 区分,同进程内所有 shell 共用一份)。
fn loop_script_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("denia-shell-loop-{}.ps1", std::process::id()))
}

/// 一个长驻 shell 进程。
pub struct PersistentShell {
    dialect: Dialect,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    dead: AtomicBool,
    seq: AtomicU64,
}

impl PersistentShell {
    fn spawn(dialect: Dialect, cwd: &std::path::Path) -> Result<Self, String> {
        let mut command = match dialect {
            Dialect::PowerShell => {
                let executable = shell::shell_executable_path();
                // 把循环写成临时脚本文件用 `-File` 起,而不是 `-Command <脚本>`:
                // 后者要经过 Windows 命令行参数转义(Rust 会包双引号并把内部 `"`
                // 转成 `\"`,而 PowerShell 的参数解析规则与 MSVCRT 不同),脚本被
                // 解析坏时子进程起来就死,报错还只会是"管道正在被关闭"。落成文件
                // 之后参数里只有一个路径,stdin 也完全归我们。
                let script = loop_script_path();
                std::fs::write(&script, PWSH_LOOP)
                    .map_err(|error| format!("写持久 shell 循环脚本失败:{error}"))?;
                let mut command = Command::new(executable);
                command.args(["-NoLogo", "-NoProfile", "-File"]).arg(&script);
                command
            }
            Dialect::Posix => {
                // 裸起 bash:管道 stdin 会被当逐行输入,状态天然保留。
                Command::new("bash")
            }
        };
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("持久 shell 启动失败:{error}"))?;
        let stdin = child.stdin.take();
        let shared = Arc::new((
            Mutex::new(Shared {
                bytes: Vec::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, Arc::clone(&shared));
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, Arc::clone(&shared));
        }
        Ok(Self {
            dialect,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            shared,
            dead: AtomicBool::new(false),
            seq: AtomicU64::new(0),
        })
    }

    /// 在常驻 shell 里跑一条命令,等它结束或超时。
    ///
    /// 超时意味着进程状态不可知(可能卡在半条语句里),直接杀掉并置
    /// `dead`,下一次调用会重新起一个干净 shell——不做"猜它缓过来了没有"
    /// 的补救。
    pub fn run(&self, command: &str, timeout: Duration) -> Result<Captured, String> {
        if self.dead.load(Ordering::Acquire) {
            return Err("持久 shell 已终止".to_string());
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let tag = format!("{}-{}", std::process::id(), seq);
        let start = format!("__DENIA_SHELL_START_{tag}__");
        let end = format!("__DENIA_SHELL_END_{tag}:");
        let line = match self.dialect {
            // 标记由常驻循环生成;这里只给 `<nonce>\t<命令>`。
            Dialect::PowerShell => format!("{tag}\t{}", escape_powershell(command)),
            // bash 自己就把管道 stdin 当逐行输入,标记直接写在命令里(抄 dsh)。
            Dialect::Posix => wrap_posix(command, &start, &end),
        };
        {
            let mut guard = self.stdin.lock().unwrap();
            let stdin = guard.as_mut().ok_or("持久 shell 已关闭")?;
            // 上面那行必须是**物理单行**,这里补的换行才是这条命令的终止符。
            stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush())
                .map_err(|error| {
                    self.dead.store(true, Ordering::Release);
                    format!("写入持久 shell 失败:{error}")
                })?;
        }

        let (lock, cvar) = &*self.shared;
        let deadline = Instant::now() + timeout;
        let mut state = lock.lock().unwrap();
        loop {
            if let Some(captured) = extract(&state.bytes, &start, &end) {
                return Ok(captured);
            }
            if state.closed {
                self.dead.store(true, Ordering::Release);
                return Err(format!(
                    "持久 shell 进程已退出(命令未返回结果)\n建议:下一条命令会自动重开一个干净 shell"
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                drop(state);
                self.kill();
                return Err(format!(
                    "命令超时({} ms 未结束)\n建议:该会话的持久 shell 已终止,下一条命令会重开一个干净 shell;长任务请用 run_in_background,或把逻辑写成脚本文件再执行",
                    timeout.as_millis()
                ));
            }
            let (next, _) = cvar.wait_timeout(state, deadline - now).unwrap();
            state = next;
        }
    }

    /// 杀掉常驻进程(超时/中断/收尾)。幂等。
    pub fn kill(&self) {
        self.dead.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// 进程是否还能用(健康检查用,不产生副作用)。
    pub fn is_alive(&self) -> bool {
        !self.dead.load(Ordering::Acquire)
    }
}

impl Drop for PersistentShell {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 常驻 shell 注册表:按会话 id 归属,一个会话一个 shell。
#[derive(Clone, Default)]
pub struct ShellHub {
    inner: Arc<HubInner>,
}

#[derive(Default)]
struct HubInner {
    shells: Mutex<HashMap<String, Arc<PersistentShell>>>,
}

impl ShellHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取该会话的常驻 shell;没有、或上一个已经死掉,就新起一个。
    pub fn get_or_spawn(
        &self,
        key: &str,
        cwd: &std::path::Path,
    ) -> Result<Arc<PersistentShell>, String> {
        let mut shells = self.inner.shells.lock().unwrap();
        if let Some(shell) = shells.get(key) {
            if shell.is_alive() {
                return Ok(Arc::clone(shell));
            }
            shells.remove(key);
        }
        let shell = Arc::new(PersistentShell::spawn(Dialect::detect(), cwd)?);
        shells.insert(key.to_string(), Arc::clone(&shell));
        Ok(shell)
    }

    /// 丢弃该会话的 shell(杀掉进程)。会话结束/删除时调用,防常驻进程泄漏。
    pub fn close(&self, key: &str) {
        let mut shells = self.inner.shells.lock().unwrap();
        if let Some(shell) = shells.remove(key) {
            shell.kill();
        }
    }

    /// 丢弃全部 shell(进程退出前调用)。
    pub fn close_all(&self) {
        let mut shells = self.inner.shells.lock().unwrap();
        for (_, shell) in shells.drain() {
            shell.kill();
        }
    }

    /// 当前常驻 shell 数量(测试与诊断用)。
    pub fn live_count(&self) -> usize {
        self.inner.shells.lock().unwrap().len()
    }
}

/// 后台泵线程:把子进程的 stdout/stderr 累积进共享缓冲并唤醒等待者。
///
/// 两个流合并进同一份缓冲(与 dsh 的 PTY 单流一致):模型看到的是"命令的输出",
/// 不需要自己判断哪一行来自 stderr。
fn pump<R: Read + Send + 'static>(reader: R, shared: Arc<(Mutex<Shared>, Condvar)>) {
    let mut reader = reader;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let (lock, cvar) = &*shared;
                let mut state = lock.lock().unwrap();
                state.bytes.extend_from_slice(&chunk[..count]);
                trim_scrollback(&mut state.bytes);
                cvar.notify_all();
            }
        }
    }
    let (lock, cvar) = &*shared;
    lock.lock().unwrap().closed = true;
    cvar.notify_all();
}

/// 只保留尾部。优先切在换行处,保证留下的是完整行;但一条超长行把缓冲撑满时
/// 无行可切,这时宁可切在这行中间也不能把输出清空——模型宁可看到半行,也不能
/// 看到空。
fn trim_scrollback(bytes: &mut Vec<u8>) {
    if bytes.len() <= SCROLLBACK_CAP {
        return;
    }
    let cut = bytes.len() - SCROLLBACK_CAP;
    let keep_from = match bytes[cut..].iter().position(|byte| *byte == b'\n') {
        // 换行落在尾巴上就没东西可留了,退回按字节切。
        Some(offset) if offset + 1 < SCROLLBACK_CAP => cut + offset + 1,
        _ => cut,
    };
    bytes.drain(..keep_from);
}

/// 从累积输出里提取本次命令的结果。
fn extract(bytes: &[u8], start: &str, end: &str) -> Option<Captured> {
    let text = String::from_utf8_lossy(bytes);
    let end_at = text.rfind(end)?;
    let digits: String = text[end_at + end.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let exit_code = digits.parse::<i32>().ok()?;
    let start_at = text[..end_at].rfind(start)?;
    let body = &text[start_at + start.len()..end_at];
    Some(Captured {
        text: trim_blank_edges(body),
        exit_code: Some(exit_code),
    })
}

fn trim_blank_edges(text: &str) -> String {
    let text = text.strip_prefix("\r\n").unwrap_or_else(|| text.strip_prefix('\n').unwrap_or(text));
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
        .to_string()
}

/// 嵌进 PowerShell 双引号字符串所需的转义(常驻循环会把命令包进 `"` 再
/// `Invoke-Expression`)。
///
/// 去 `\r`、把 `\n` 折成 `` `n `` 是为了让整条输入保持物理单行——多行会被
/// 当续行,提示符泄漏进模型看到的结果。
fn escape_powershell(command: &str) -> String {
    let mut out = String::with_capacity(command.len() + 8);
    for ch in command.chars() {
        match ch {
            '`' => out.push_str("``"),
            '"' => out.push_str("`\""),
            '$' => out.push_str("`$"),
            '\r' => {}
            '\n' => out.push_str("`n"),
            '\u{1b}' => out.push_str("`e"),
            other => out.push(other),
        }
    }
    out
}

/// POSIX 包装:同样一条物理单行,命令用 `$'...'` 引号保证换行不落进 stdin 行。
fn wrap_posix(command: &str, start: &str, end: &str) -> String {
    format!(
        "printf '%s\\n' {start}; eval {command}; __denia_s=$?; printf '%s%s\\n' {end} \"$__denia_s\"",
        start = quote_posix(start),
        end = quote_posix(end),
        command = quote_posix(command),
    )
}

fn quote_posix(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    out.push_str("$'");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上面这些 `#[ignore]` 的用例要真起一个 PowerShell 长驻进程,
    /// **在当前沙盒里做不到**:带管道 stdin 调 `CreateProcess` 启动
    /// powershell.exe / pwsh.exe 会卡在 `spawn()` 不返回。同一沙盒里
    /// `cmd /c` 带管道 stdin 完全正常,而现有一次性 bash 之所以没事,是因为它
    /// 用 `Stdio::null()`。等换到不受沙盒约束的宿主(或改用套接字/命名管道
    /// 传命令)再跑这些用例。诊断入口见 `diagnose_child_exit`。

    /// 标记提取是纯函数,可以在这里验:它决定"命令结束没结束、退出码几"。
    #[test]
    fn extract_pulls_body_and_exit_code_between_markers() {
        let bytes = b"prefix junk\n__DENIA_SHELL_START_7-1__\nhello\nworld\n__DENIA_SHELL_END_7-1:0\n";
        let captured = extract(bytes, "__DENIA_SHELL_START_7-1__", "__DENIA_SHELL_END_7-1:").unwrap();
        assert_eq!(captured.text, "hello\nworld");
        assert_eq!(captured.exit_code, Some(0));
        // 换行用 \r\n 也要认(Windows 输出)。
        let crlf = b"__DENIA_SHELL_START_7-2__\r\nbody\r\n__DENIA_SHELL_END_7-2:3\r\n";
        let captured = extract(crlf, "__DENIA_SHELL_START_7-2__", "__DENIA_SHELL_END_7-2:").unwrap();
        assert_eq!(captured.text, "body");
        assert_eq!(captured.exit_code, Some(3));
        // 标记还没出现 → None(调用方继续等)。
        assert!(extract(b"still running", "__S__", "__E__:").is_none());
        // 有结束标记没有开始标记 → None,不能拿半截输出当结果。
        assert!(extract(b"__E__:0", "__S__", "__E__:").is_none());
    }

    /// 诊断用:`cargo test -p denia-tools --lib -- --ignored --nocapture diagnose_child_exit`
    #[test]
    #[ignore]
    fn diagnose_child_exit() {
        let script = loop_script_path();
        std::fs::write(&script, PWSH_LOOP).unwrap();
        // 先排除"沙盒下带管道 stdin 建进程"这个变量:换一个平凡子进程试试。
        eprintln!("[control] cmd /c echo hi with piped stdin");
        match Command::new("cmd")
            .args(["/c", "echo hi"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut control) => {
                let mut stdin = control.stdin.take().unwrap();
                match stdin.write_all(b"x\n") {
                    Ok(()) => eprintln!("  control write ok"),
                    Err(error) => eprintln!("  control write FAILED: {error}"),
                }
                let out = control.wait_with_output();
                eprintln!("  control output = {:?}", out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()));
            }
            Err(error) => eprintln!("  control spawn failed: {error}"),
        }

        let candidates: Vec<(&str, std::path::PathBuf)> = vec![
            (
                "system32",
                std::path::PathBuf::from(
                    r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                ),
            ),
            ("alias", shell::shell_executable_path()),
        ];
        for (label, exe) in candidates {
            eprintln!("[{label}] {} exists={}", exe.display(), exe.is_file());
            if !exe.is_file() {
                continue;
            }
            let mut child = match Command::new(&exe)
                .args(["-NoLogo", "-NoProfile", "-File"])
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    eprintln!("  spawn failed: {error}");
                    continue;
                }
            };
            let mut stdin = child.stdin.take().unwrap();
            let shared = Arc::new((
                Mutex::new(Shared {
                    bytes: Vec::new(),
                    closed: false,
                }),
                Condvar::new(),
            ));
            if let Some(out) = child.stdout.take() {
                pump(out, Arc::clone(&shared));
            }
            if let Some(err) = child.stderr.take() {
                pump(err, Arc::clone(&shared));
            }
            std::thread::sleep(Duration::from_millis(900));
            eprintln!("  try_wait = {:?}", child.try_wait());
            match stdin.write_all(b"t1\tWrite-Output 'hi'\n") {
                Ok(()) => {
                    eprintln!("  write ok");
                    std::thread::sleep(Duration::from_millis(900));
                    eprintln!(
                        "  output = {:?}",
                        String::from_utf8_lossy(&shared.0.lock().unwrap().bytes)
                    );
                }
                Err(error) => {
                    eprintln!("  write FAILED: {error}");
                    eprintln!("  try_wait = {:?}", child.try_wait());
                    eprintln!(
                        "  backlog = {:?}",
                        String::from_utf8_lossy(&shared.0.lock().unwrap().bytes)
                    );
                }
            }
            let _ = child.kill();
        }
    }

    fn shell_for_tests() -> (ShellHub, String) {
        // pid + 进程内序号:并行测试各拿各的目录,不靠时钟精度。
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "denia-shell-session-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (ShellHub::new(), dir.to_string_lossy().to_string())
    }

    fn run(hub: &ShellHub, key: &str, cwd: &str, command: &str) -> Captured {
        let shell = hub.get_or_spawn(key, std::path::Path::new(cwd)).unwrap();
        shell
            .run(command, Duration::from_secs(30))
            .unwrap_or_else(|error| panic!("run failed: {error}"))
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn state_persists_across_calls() {
        let (hub, cwd) = shell_for_tests();
        let set = if cfg!(windows) {
            "$denia_probe = 41; Write-Output 'set'"
        } else {
            "denia_probe=41; echo set"
        };
        let read = if cfg!(windows) {
            "Write-Output (\"probe=\" + $denia_probe)"
        } else {
            "echo \"probe=$denia_probe\""
        };
        let first = run(&hub, "s1", &cwd, set);
        assert_eq!(first.exit_code, Some(0), "{}", first.text);
        let second = run(&hub, "s1", &cwd, read);
        assert!(
            second.text.contains("probe=41"),
            "变量没有跨调用保留:{}",
            second.text
        );
        hub.close_all();
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn working_directory_persists_across_calls() {
        let (hub, cwd) = shell_for_tests();
        let sub = std::path::Path::new(&cwd).join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let cd = format!("Set-Location '{}'", sub.to_string_lossy());
        let cd = if cfg!(windows) {
            cd
        } else {
            format!("cd '{}'", sub.to_string_lossy())
        };
        run(&hub, "s2", &cwd, &cd);
        let pwd = if cfg!(windows) {
            "(Get-Location).Path"
        } else {
            "pwd"
        };
        let after = run(&hub, "s2", &cwd, pwd);
        assert!(
            after.text.contains("nested"),
            "工作目录没有跨调用保留:{}",
            after.text
        );
        hub.close_all();
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn exit_code_is_captured_and_shell_survives_failure() {
        let (hub, cwd) = shell_for_tests();
        let fail = if cfg!(windows) { "cmd /c exit 7" } else { "exit 7" };
        let failed = run(&hub, "s3", &cwd, fail);
        // POSIX 的 `exit 7` 会退出 shell 本身,单独断言下面另测。
        if cfg!(windows) {
            assert_eq!(failed.exit_code, Some(7), "{}", failed.text);
            let ok = run(&hub, "s3", &cwd, "Write-Output 'alive'");
            assert!(ok.text.contains("alive"), "失败后 shell 应仍然存活");
        }
        hub.close_all();
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn multiline_command_executes_as_one_unit() {
        let (hub, cwd) = shell_for_tests();
        let command = if cfg!(windows) {
            "Write-Output 'line1'\nWrite-Output 'line2'"
        } else {
            "echo line1\necho line2"
        };
        let out = run(&hub, "s4", &cwd, command);
        assert!(out.text.contains("line1"), "{}", out.text);
        assert!(out.text.contains("line2"), "{}", out.text);
        hub.close_all();
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn marker_text_in_output_does_not_confuse_extraction() {
        let (hub, cwd) = shell_for_tests();
        // 命令自己打印一个像标记的串:nonce 唯一,不该被误判为结束。
        let out = run(
            &hub,
            "s5",
            &cwd,
            "Write-Output '__DENIA_SHELL_END_0-0:999'",
        );
        assert!(out.text.contains("__DENIA_SHELL_END_0-0:999"), "{}", out.text);
        hub.close_all();
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn sessions_are_isolated_from_each_other() {
        let (hub, cwd) = shell_for_tests();
        if cfg!(windows) {
            run(&hub, "a", &cwd, "$isolated = 'only-a'");
            let b = run(&hub, "b", &cwd, "Write-Output (\"got=\" + $isolated)");
            assert!(
                b.text.contains("got="),
                "另一个会话不该看到这个变量:{}",
                b.text
            );
            assert!(!b.text.contains("only-a"), "会话之间必须隔离:{}", b.text);
        }
        assert_eq!(hub.live_count(), if cfg!(windows) { 2 } else { 0 });
        hub.close("a");
        hub.close_all();
        assert_eq!(hub.live_count(), 0, "close_all 之后不该残留常驻 shell");
    }

    #[test]
    #[ignore = "沙盒环境下 PowerShell 带管道 stdin 建进程会挂(tests 模块注释)"]
    fn timeout_kills_the_shell_and_next_call_respawns() {
        let (hub, cwd) = shell_for_tests();
        let shell = hub.get_or_spawn("t", std::path::Path::new(&cwd)).unwrap();
        let sleep = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let timed_out = shell.run(sleep, Duration::from_millis(300));
        assert!(timed_out.is_err(), "超时必须报错而不是干等");
        assert!(!shell.is_alive(), "超时后该 shell 应被标记为死亡");
        // 下一次调用要到新 shell,而且能正常工作。
        let fresh = run(&hub, "t", &cwd, "Write-Output 'fresh'");
        assert!(fresh.text.contains("fresh"), "{}", fresh.text);
        hub.close_all();
    }

    #[test]
    fn scrollback_trims_to_cap_on_line_boundary() {
        // 多行输出:切在换行处,留下完整行。
        let mut bytes = Vec::new();
        for _ in 0..(SCROLLBACK_CAP / 8 + 64) {
            bytes.extend_from_slice(b"1234567\n");
        }
        let total = bytes.len();
        trim_scrollback(&mut bytes);
        assert!(total > bytes.len(), "超限了就该裁");
        assert!(bytes.len() <= SCROLLBACK_CAP, "裁完还超限:{}", bytes.len());
        assert!(
            bytes.len() > SCROLLBACK_CAP - 16,
            "裁得太狠,丢多了:{}",
            bytes.len()
        );
        assert_eq!(bytes.first(), Some(&b'1'), "必须从整行开头起");
        assert_eq!(bytes.last(), Some(&b'\n'));

        // 一条超长行把缓冲撑满:无行可切,但不能把输出清空。
        let mut huge = vec![b'x'; SCROLLBACK_CAP + 100];
        trim_scrollback(&mut huge);
        assert_eq!(huge.len(), SCROLLBACK_CAP);
        assert_eq!(huge.last(), Some(&b'x'), "宁可切在行中间也不能清空");
    }

    #[test]
    fn powershell_escaping_keeps_input_on_one_line() {
        let escaped = escape_powershell("Write-Output \"a\"\nWrite-Output $env:PATH");
        assert!(!escaped.contains('\n'), "换行必须折成 `n:{escaped}");
        assert!(escaped.contains("`n"), "{escaped}");
        assert!(escaped.contains("`\""), "{escaped}");
        assert!(escaped.contains("`$env:PATH"), "{escaped}");
    }

    #[test]
    fn posix_quoting_keeps_wrapper_on_one_line() {
        let wrapped = wrap_posix("echo 'a'\necho $HOME", "__S__", "__E__:");
        assert!(!wrapped.contains('\n'), "包装必须是物理单行:{wrapped}");
        assert!(wrapped.contains("\\n"), "{wrapped}");
    }
}
