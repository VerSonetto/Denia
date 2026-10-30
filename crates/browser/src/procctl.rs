//! 浏览器实例的进程归属与回收。
//!
//! # 为什么需要这一层
//!
//! chromiumoxide 只在 `Browser` 句柄被 drop 时靠 `kill_on_drop` 收进程,而
//! "关掉最后一个标签页"不会 drop 句柄,宿主被强杀时更不会跑析构。于是 Chrome
//! 留在后台,占着 `--user-data-dir` 的**单例锁**:下一次冷启动的新 Chrome 一
//! 发现锁被持有,就把请求转交给老实例然后自行退出,denia 拿不到 WebSocket 调试
//! 地址,表现为 `before websocket URL could be resolved` 的启动失败 —— 控制面
//! 以为没在跑、进程却卡着占资源,两头都对不上。
//!
//! 本模块补三件事:
//!
//! 1. **归属**:[`claim`] 落一份 `<home>/browser/.instance.json`,记 Chrome 主
//!    进程 PID、镜像路径、进程创建时间与写者(本 denia)PID;返回的
//!    [`ProfileToken`] 活着即代表"本会话正占用这个 profile"。
//! 2. **回收**:[`kill_tree`] 按 PID 连根拔进程树(Windows 上 tokio 的 `kill`
//!    只 `TerminateProcess` 直接子进程,gpu/network/crashpad 等后代不受影响)。
//! 3. **兜底清扫**:[`reap_stale`] 在冷启动前清掉上一代没退干净的残留。
//!
//! 误杀防护:残留判定要求 **PID 存活 + 镜像路径相同 + 进程创建时间相同**三条
//! 同时成立;PID 复用要撞上同一路径与同一创建时间的概率可以忽略。写者是**别的**
//! denia 且仍活着时一律不强杀,只报告冲突(两个实例抢同一 profile 时,报错比
//! 抢占更安全)。写者是本进程时必定是自己的上一代会话 —— [`reap_stale`] 只在
//! 无活跃会话的路径上被调用,此时那个 Chrome 就是残留,该收。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 归属记录(锁文件内容)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// Chrome 主进程 PID。
    chrome_pid: u32,
    /// Chrome 可执行文件路径(与 PID 一起构成身份)。
    exe: String,
    /// Chrome 进程创建时间(epoch ms);PID 复用时这一项必然不同。
    /// 采不到时写 0,比对端要求两侧同为 0 才认可(见 [`record_matches`])。
    created_ms: u64,
    /// 写这条记录的 denia 进程 PID:别的实例还活着说明 profile 正被使用。
    writer_pid: u32,
    /// 记录写入时刻(epoch ms),仅用于日志。
    written_ms: u64,
}

/// 进程探测结果:`None` 表示进程不存在或读不到(视为已退出)。
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// 主镜像完整路径(取不到时为 None,不参与比对)。
    pub exe: Option<String>,
    /// 进程创建时间(epoch ms;取不到时为 None)。
    pub created_ms: Option<u64>,
}

/// profile 归属令牌:活着代表本会话正占用这个 profile,drop 时释放记录。
///
/// 只删属于自己那一代 PID 的记录:同进程里新会话已经改写记录时,旧令牌 drop
/// 不得把新记录带走。
pub struct ProfileToken {
    path: Option<PathBuf>,
    chrome_pid: Option<u32>,
}

impl ProfileToken {
    /// 不持有任何归属的空令牌(进程已收干净、但结构体还得存在时用)。
    pub fn detached() -> Self {
        Self {
            path: None,
            chrome_pid: None,
        }
    }

    /// 丢掉令牌但**保留**记录。
    ///
    /// 给「强杀也没能把 Chrome 收掉」的异常路径用:记录在,下一次冷启动的
    /// [`reap_stale`] 才能按 PID + 身份再试一次;正常回收不走这里。
    pub fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for ProfileToken {
    fn drop(&mut self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let Some(mine) = self.chrome_pid else {
            return;
        };
        if let Some(record) = read(path)
            && record.chrome_pid != mine
        {
            // 记录已被更晚的会话改写,不归我删。
            return;
        }
        remove(path);
    }
}

/// 锁文件路径:与 profile 同级,避免混进 Chrome 自己的用户数据里。
pub fn path_for(profile_dir: &Path) -> PathBuf {
    profile_dir
        .parent()
        .map(|dir| dir.join(".instance.json"))
        .unwrap_or_else(|| profile_dir.join(".denia-instance.json"))
}

fn read(path: &Path) -> Option<Record> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn normalize_path(raw: &str) -> String {
    raw.trim().replace('\\', "/").to_ascii_lowercase()
}

/// 记录里的 Chrome 是否**就是**当前那个同 PID 进程(身份比对)。
fn record_matches(record: &Record, probe: &Probe) -> bool {
    let exe_ok = match &probe.exe {
        Some(probe_exe) => normalize_path(probe_exe) == normalize_path(&record.exe),
        // 读不到镜像路径(权限/正在退出):宁可不动手。
        None => false,
    };
    let created_ok = match probe.created_ms {
        // 容忍 2s 的读取精度差;PID 复用不会落在同一秒内。
        Some(ms) => ms.abs_diff(record.created_ms) <= 2_000,
        // 探测端拿不到创建时间(Unix/macOS):只认"记录端也没写"的情形,
        // 此时身份退化为 PID + 镜像路径。
        None => record.created_ms == 0,
    };
    exe_ok && created_ok
}

/// 记录指向的 Chrome 是否确实还在运行(且身份相符)。
fn chrome_alive(record: &Record) -> bool {
    describe(record.chrome_pid)
        .as_ref()
        .is_some_and(|probe| record_matches(record, probe))
}

/// 写者是否为**另一个**仍存活的 denia 进程。
fn held_by_other_instance(record: &Record) -> bool {
    if record.writer_pid == std::process::id() {
        return false;
    }
    // 写者 PID 复用会误判成"还活着",方向上偏保守(拒绝强杀),可接受。
    describe(record.writer_pid).is_some()
}

/// 采集 Chrome 主进程身份(写记录用);拿不到 PID 时退化为可执行文件路径。
fn identity(chrome_pid: Option<u32>, fallback_exe: &Path) -> (String, u64) {
    let probe = chrome_pid.and_then(describe);
    match probe {
        Some(probe) => (
            probe
                .exe
                .unwrap_or_else(|| fallback_exe.to_string_lossy().into_owned()),
            probe.created_ms.unwrap_or(0),
        ),
        None => (fallback_exe.to_string_lossy().into_owned(), 0),
    }
}

/// 登记本实例对 profile 的占用,返回归属令牌。
///
/// 写失败只意味着下次无法精确回收残留(不影响本次会话),记 warn 后继续。
pub fn claim(profile_dir: &Path, chrome_pid: Option<u32>, exe: &Path) -> ProfileToken {
    let path = path_for(profile_dir);
    let (exe_text, created_ms) = identity(chrome_pid, exe);
    let record = Record {
        chrome_pid: chrome_pid.unwrap_or(0),
        exe: exe_text,
        created_ms,
        writer_pid: std::process::id(),
        written_ms: now_ms(),
    };
    if let Err(error) = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&record).unwrap_or_default(),
    ) {
        tracing::warn!("写浏览器实例记录失败({}): {error}", path.display());
        return ProfileToken::detached();
    }
    ProfileToken {
        path: Some(path),
        chrome_pid,
    }
}

/// 回收结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// 没有残留。
    Clean,
    /// 只清了一条指向已消失进程的记录。
    StaleRecord,
    /// 连根拔掉了一个残留 Chrome 进程树。
    ProcessTree,
}

/// 冷启动前清扫上一代残留。
///
/// 返回 `Err` 表示"别的 denia 实例正占用这个 profile 且不能动它"。调用方应把
/// 它转成可读的启动失败原因,而不是继续 spawn —— 那会以 `ExitStatus(21)` 这种
/// 看不出所以然的形态失败。
pub async fn reap_stale(profile_dir: &Path) -> Result<Reaped, String> {
    let path = path_for(profile_dir);
    let Some(record) = read(&path) else {
        return Ok(Reaped::Clean);
    };
    if !chrome_alive(&record) {
        remove(&path);
        return Ok(if describe(record.chrome_pid).is_some() {
            // PID 还活着但身份不符 → 已被别的进程复用,原 Chrome 早退了。
            Reaped::StaleRecord
        } else {
            Reaped::Clean
        });
    }
    if held_by_other_instance(&record) {
        return Err(format!(
            "该 profile 正被另一个 denia 实例使用(其浏览器进程 {} 仍在运行)。\
             同一 profile 不能同时被两个实例驱动:请在同一个 denia 里用浏览器,\
             或先让那个实例回收(releaseBrowser/shutdownBrowser)",
            record.chrome_pid
        ));
    }
    tracing::info!("清扫上次残留的浏览器进程: pid={}", record.chrome_pid);
    kill_tree(record.chrome_pid, Some(Path::new(&record.exe))).await;
    if !wait_gone(record.chrome_pid, Duration::from_secs(5)).await {
        return Err(format!(
            "残留的浏览器进程 {} 结束不掉,profile 仍被其占用;请手动结束该进程后重试",
            record.chrome_pid
        ));
    }
    remove(&path);
    Ok(Reaped::ProcessTree)
}

/// 本实例回收自己拉起的 Chrome:按 PID 连根拔进程树并等它消失。
///
/// 与 [`reap_stale`] 的区别:这里明知是自己的进程(会话正在收),不做身份比对。
///
/// 返回值:主进程是否确实消失了。`false` 时调用方**不要**释放归属记录,留给
/// 下次冷启动再清算一次(比默默留下无人认领的进程好)。
pub async fn reap_own(chrome_pid: Option<u32>, exe_hint: Option<&Path>) -> bool {
    let Some(pid) = chrome_pid.filter(|pid| *pid != 0) else {
        // 拿不到 PID:没有可回收的东西,视为已干净。
        return true;
    };
    kill_tree(pid, exe_hint).await;
    let gone = wait_gone(pid, Duration::from_secs(5)).await;
    if !gone {
        tracing::warn!("浏览器进程 {pid} 在 5s 内未消失,可能有子进程残留");
    }
    gone
}

/// 按 PID 连根拔一棵进程树。
///
/// Windows 上 tokio 的 `kill` 只 `TerminateProcess` 直接子进程,gpu/network/
/// crashpad 等后代留在后台继续占资源;`taskkill /T /F` 能连根拔,但**父进程
/// 已先死**时它找不到这棵树。所以先按「创建者 PID」把子孙列出来(Windows 的
/// `th32ParentProcessID` 在父进程死后仍保留在子进程里),再逐层收。
///
/// `exe_hint` 是浏览器主程序路径:扫描到的同名进程才顺手收,避免 PID 复用时
/// 误杀无关进程。
///
/// 与 `denia_tools::shell::kill_tree` 同一手法(browser crate 不能反向依赖
/// tools,故本地实现一份)。
#[cfg(windows)]
pub async fn kill_tree(pid: u32, exe_hint: Option<&Path>) {
    use tokio::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let descendants = descendant_pids(pid, exe_hint);
    let mut command = Command::new("taskkill.exe");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW);
    let _ = command.status().await;

    // 父进程已死时上面的 /T 扫不到东西,逐个补收同镜像的后代。
    for child in descendants {
        if describe(child).is_none() {
            continue;
        }
        let mut command = Command::new("taskkill.exe");
        command
            .args(["/PID", &child.to_string(), "/F"])
            .creation_flags(CREATE_NO_WINDOW);
        let _ = command.status().await;
    }
}

#[cfg(not(windows))]
pub async fn kill_tree(pid: u32, _exe_hint: Option<&Path>) {
    use tokio::process::Command;
    for child in descendant_pids(pid, _exe_hint) {
        let _ = Command::new("kill")
            .args(["-KILL", &child.to_string()])
            .status()
            .await;
    }
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .await;
}

/// 列出「由 `pid` 创建、镜像与浏览器主程序同名」的子孙进程(含间接后代)。
///
/// 只向下走一层快照并按父 PID 递归;深度上限是防环与异常进程树。
#[cfg(windows)]
fn descendant_pids(root: u32, exe_hint: Option<&Path>) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let wanted = exe_hint.and_then(exe_basename);
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Vec::new();
    }

    // (父, 子, 镜像名基名) 清单。用 W 版枚举:ANSI 版在中文路径上会退化。
    let mut rows: Vec<(u32, u32, Option<String>)> = Vec::new();
    unsafe {
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let name = lower_utf16(&entry.szExeFile);
                rows.push((entry.th32ParentProcessID, entry.th32ProcessID, Some(name)));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }

    let mut found = Vec::new();
    let mut frontier = vec![root];
    for _ in 0..8 {
        let mut next = Vec::new();
        for parent in frontier {
            for (row_parent, child, name) in &rows {
                if *row_parent != parent || *child == parent {
                    continue;
                }
                // 护栏:只收浏览器镜像(gpu/utility/crashpad 都是同一个 exe)。
                if let (Some(wanted), Some(name)) = (wanted.as_deref(), name.as_deref())
                    && name != wanted
                {
                    continue;
                }
                if !found.contains(child) {
                    found.push(*child);
                    next.push(*child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    found
}

/// Linux 按 `/proc/<pid>/stat` 第 4 项(ppid)向下递归;镜像名由 `exe` 链接基名比对。
#[cfg(unix)]
fn descendant_pids(root: u32, exe_hint: Option<&Path>) -> Vec<u32> {
    let wanted = exe_hint.and_then(exe_basename);
    let mut all: Vec<(u32, u32, Option<String>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // 格式:pid (comm) state ppid ... —— comm 可含空格,从最后一个 ')' 之后取字段。
        let Some(tail) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[tail + 1..].split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        all.push((
            ppid,
            pid,
            wanted.as_ref().map(|_| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .ok()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().to_ascii_lowercase())
                    })
                    .unwrap_or_default()
            }),
        ));
    }
    let mut found = Vec::new();
    let mut frontier = vec![root];
    for _ in 0..8 {
        let mut next = Vec::new();
        for parent in frontier {
            for (ppid, pid, name) in &all {
                if *ppid != parent || *pid == parent || found.contains(pid) {
                    continue;
                }
                if let (Some(wanted), Some(name)) = (wanted.as_deref(), name.as_deref())
                    && !name.is_empty()
                    && name != wanted
                {
                    continue;
                }
                found.push(*pid);
                next.push(*pid);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    found
}

/// 取镜像基名(小写),用于只收浏览器进程。
#[cfg(windows)]
fn exe_basename(exe: &Path) -> Option<String> {
    Some(
        exe.file_name()?
            .to_string_lossy()
            .trim()
            .to_ascii_lowercase(),
    )
}

#[cfg(windows)]
fn lower_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|unit| *unit == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end]).to_ascii_lowercase()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn exe_basename(exe: &Path) -> Option<String> {
    Some(
        exe.file_name()?
            .to_string_lossy()
            .trim()
            .to_ascii_lowercase(),
    )
}

/// macOS 探测不到身份,也就不能安全地按名字扫进程树。
#[cfg(target_os = "macos")]
fn exe_basename(_exe: &Path) -> Option<String> {
    None
}

/// 该 profile 是否仍被一个活着的浏览器进程占用(只读:不回收、不抹记录)。
///
/// 它是「收尾到底收干净没有」的唯一真口径:看控制面状态不算(进程可以活在
/// 控制面之外),翻进程列表又不区分是不是本实例拉起的。
pub fn held(profile_dir: &Path) -> bool {
    read(&path_for(profile_dir)).is_some_and(|record| chrome_alive(&record))
}

/// 等进程消失(Chrome 释放单例锁要到进程真正回收之后)。
pub async fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if describe(pid).is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

/// 探测进程是否存在,并尽量带回镜像路径与创建时间。
#[cfg(windows)]
pub fn describe(pid: u32) -> Option<Probe> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    const STILL_ACTIVE: u32 = 259;
    /// 1601-01-01 → 1970-01-01 之间相差的 100ns 刻度数。
    const WINDOWS_EPOCH_OFFSET: u64 = 116_444_736_000;

    if pid == 0 {
        return None;
    }
    let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut probe = Probe::default();
    let alive = unsafe {
        let mut exit_code: u32 = 0;
        if GetExitCodeProcess(handle, &mut exit_code) == 0 || exit_code != STILL_ACTIVE {
            false
        } else {
            let mut buffer = vec![0u16; 1024];
            let mut size = buffer.len() as u32;
            if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) != 0 {
                probe.exe = Some(String::from_utf16_lossy(&buffer[..size as usize]));
            }
            // FILETIME 是 1601 起的 100ns 刻度;exit/kernel 与 user/kernel 成对,
            // 创建时间只能取第一个出参。
            let mut creation = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let mut exit = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let mut kernel = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let mut user = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                let ticks =
                    ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
                probe.created_ms = Some(ticks / 10_000 - WINDOWS_EPOCH_OFFSET);
            }
            true
        }
    };
    unsafe { CloseHandle(handle) };
    alive.then_some(probe)
}

/// Linux 走 `/proc`:镜像路径由 `exe` 符号链接给出,创建时间换算易错,留空
/// (比对端退化为 PID + 路径)。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn describe(pid: u32) -> Option<Probe> {
    let proc_dir = Path::new("/proc").join(pid.to_string());
    if !proc_dir.exists() {
        return None;
    }
    Some(Probe {
        exe: std::fs::read_link(proc_dir.join("exe"))
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        created_ms: None,
    })
}

/// macOS 没有 `/proc`,拿不到可比对的身份:一律视为"不认识",只报冲突不强杀。
#[cfg(target_os = "macos")]
pub fn describe(_pid: u32) -> Option<Probe> {
    None
}

/// 现在时刻(epoch ms)。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁文件落在 profile 的同级目录,不混进 Chrome 用户数据里。
    #[test]
    fn lock_file_sits_beside_profile() {
        let path = path_for(Path::new("/home/u/.denia/browser/profile"));
        assert_eq!(path, PathBuf::from("/home/u/.denia/browser/.instance.json"));
    }

    /// 身份比对:路径大小写/分隔符差异不影响判定,创建时间与 PID 复用必须拦下。
    #[test]
    fn identity_requires_exe_and_creation_time() {
        let record = Record {
            chrome_pid: 4242,
            exe: r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string(),
            created_ms: 1_700_000_000_000,
            writer_pid: 1,
            written_ms: 0,
        };
        let matching = Probe {
            exe: Some(r"c:\program files\google\chrome\application\chrome.exe".to_string()),
            created_ms: Some(record.created_ms + 500),
        };
        assert!(
            record_matches(&record, &matching),
            "同路径同创建时间应判为同一进程"
        );

        let reused_pid = Probe {
            exe: matching.exe.clone(),
            created_ms: Some(record.created_ms + 60_000),
        };
        assert!(
            !record_matches(&record, &reused_pid),
            "PID 被复用(创建时间差 1 分钟)绝不能判为同一进程"
        );

        let other_exe = Probe {
            exe: Some(r"C:\Windows\System32\notepad.exe".to_string()),
            created_ms: matching.created_ms,
        };
        assert!(
            !record_matches(&record, &other_exe),
            "同 PID 不同镜像不能判为同一进程"
        );

        let unreadable = Probe {
            exe: None,
            created_ms: matching.created_ms,
        };
        assert!(
            !record_matches(&record, &unreadable),
            "读不到镜像时保守不动手"
        );
    }

    /// Unix 侧拿不到创建时间:退化为 PID + 路径,但记录端写了创建时间就不认。
    #[test]
    fn unix_identity_degrades_to_pid_and_exe() {
        let record = Record {
            chrome_pid: 7,
            exe: "/opt/chrome/chrome".to_string(),
            created_ms: 0,
            writer_pid: 1,
            written_ms: 0,
        };
        let probe = Probe {
            exe: Some("/opt/chrome/chrome".to_string()),
            created_ms: None,
        };
        assert!(record_matches(&record, &probe));

        let recorded_with_time = Record {
            created_ms: 1_700_000_000_000,
            ..record.clone()
        };
        assert!(
            !record_matches(&recorded_with_time, &probe),
            "记录里有创建时间而探测拿不到时不得强杀"
        );
    }

    /// 令牌 drop 只删自己那一代的记录。
    #[test]
    fn token_drop_only_removes_its_own_record() {
        let dir = std::env::temp_dir().join(format!("denia-procctl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let profile = dir.join("profile");
        std::fs::create_dir_all(&profile).expect("create profile dir");
        let path = path_for(&profile);

        let token = ProfileToken {
            path: Some(path.clone()),
            chrome_pid: Some(111),
        };
        write_record(&path, 111);
        assert!(path.exists(), "claim 之后记录应在");

        // 更新的会话改写了记录:旧令牌 drop 不得带走新记录。
        write_record(&path, 222);
        drop(token);
        assert!(path.exists(), "记录属于更晚的会话时不得删除");

        // 回到自己那一代:drop 应释放。
        write_record(&path, 111);
        let token = ProfileToken {
            path: Some(path.clone()),
            chrome_pid: Some(111),
        };
        drop(token);
        assert!(!path.exists(), "自己那一代的记录应在 drop 时释放");

        // 空令牌 drop 什么也不做(路径不存在也不报错)。
        drop(ProfileToken::detached());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_record(path: &Path, chrome_pid: u32) {
        let record = Record {
            chrome_pid,
            exe: "chrome".to_string(),
            created_ms: 0,
            writer_pid: std::process::id(),
            written_ms: 0,
        };
        std::fs::write(path, serde_json::to_vec(&record).expect("serialize")).expect("write");
    }

    /// PID 0 / 已消失进程必须探测为"不在",否则清扫逻辑会误以为有残留。
    #[test]
    fn describe_treats_absent_pid_as_gone() {
        assert!(describe(0).is_none(), "PID 0 不是浏览器进程");
        // 本进程必须被认成活着:身份判定的"活"分支靠它。
        let me = describe(std::process::id()).expect("本进程应可探测");
        assert!(me.exe.is_some(), "应能读到自身镜像路径");
    }

    /// 无记录 = 没占用;指向已消失进程的记录也不算占用。
    #[test]
    fn occupancy_ignores_records_pointing_nowhere() {
        let profile = temp_profile("held");
        let path = path_for(&profile);
        assert!(!held(&profile), "无记录时不得报占用");

        write_record(&path, 999_999);
        assert!(!held(&profile), "记录指向不存在的 PID 时不得报占用");

        // 拿本进程当"假浏览器":身份字段取真值,应报占用。
        let probe = describe(std::process::id()).expect("probe self");
        write_record_with(
            &path,
            std::process::id(),
            &probe.exe.expect("self exe"),
            probe.created_ms.unwrap_or(0),
        );
        assert!(held(&profile), "同 PID 同身份且进程活着时应报占用");
        let _ = std::fs::remove_dir_all(profile.parent().expect("temp root"));
    }

    /// 记录里的 PID 已被别的进程复用(镜像不同):必须判为"不占用"而不是误杀。
    #[test]
    fn reused_pid_is_never_treated_as_ours() {
        let profile = temp_profile("reused");
        // 本进程活着,但 write_record 写的镜像是 "chrome" —— 身份不符。
        write_record(&path_for(&profile), std::process::id());
        assert!(!held(&profile), "PID 存活但镜像不符时绝不能认为被自己占用");
        let _ = std::fs::remove_dir_all(profile.parent().expect("temp root"));
    }

    /// 扫不到残留时返回 Clean。
    #[tokio::test]
    async fn reap_stale_is_noop_without_record() {
        let profile = temp_profile("noop");
        assert_eq!(reap_stale(&profile).await.expect("clean"), Reaped::Clean);
        let _ = std::fs::remove_dir_all(profile.parent().expect("temp root"));
    }

    /// 写一个指向 `chrome_pid` 的记录(镜像字段故意用 "chrome",供不符身份的测试用)。
    fn write_record_with(path: &Path, chrome_pid: u32, exe: &str, created_ms: u64) {
        let record = Record {
            chrome_pid,
            exe: exe.to_string(),
            created_ms,
            writer_pid: std::process::id(),
            written_ms: 0,
        };
        std::fs::write(path, serde_json::to_vec(&record).expect("serialize")).expect("write");
    }

    /// 残留真被连根拔:假浏览器(子进程) + 本进程写的记录 → ProcessTree、进程消失、记录清掉。
    ///
    /// 跑的是与真浏览器完全同一份代码路径(chrome_alive → kill_tree → wait_gone)。
    #[tokio::test]
    #[cfg(windows)]
    async fn reap_stale_terminates_a_real_leftover_process() {
        let profile = temp_profile("reap");
        let path = path_for(&profile);
        let (pid, exe, created) = spawn_sleeper().await;
        // 写者 = 本进程:那个进程是上一代会话留下的残留,该收。
        write_record_with(&path, pid, &exe, created);

        assert!(held(&profile), "前置:应判为占用");
        assert_eq!(
            reap_stale(&profile).await.expect("reap"),
            Reaped::ProcessTree,
            "残留进程必须被回收"
        );
        assert!(
            wait_gone(pid, Duration::from_secs(5)).await,
            "回收后进程必须消失(这正是『启动不了』的根因)"
        );
        assert!(!path.exists(), "回收后记录必须清掉");
        let _ = std::fs::remove_dir_all(profile.parent().expect("temp root"));
    }

    /// 别的实例正用着这个 profile:只报冲突,不抢也不杀。
    #[tokio::test]
    #[cfg(windows)]
    async fn reap_stale_refuses_to_touch_another_instance() {
        let profile = temp_profile("conflict");
        let path = path_for(&profile);
        let (browser_pid, exe, created) = spawn_sleeper().await;
        let (writer_pid, _, _) = spawn_sleeper().await;
        let record = Record {
            chrome_pid: browser_pid,
            exe,
            created_ms: created,
            // 写者是另一个活着的进程:不能断定那个浏览器是孤儿。
            writer_pid,
            written_ms: 0,
        };
        std::fs::write(&path, serde_json::to_vec(&record).expect("serialize")).expect("write");

        let error = reap_stale(&profile)
            .await
            .expect_err("被别的实例占用时应报错,而不是去 spawn一个必然失败的 Chrome");
        assert!(
            error.contains("另一个 denia 实例"),
            "错误要说清是谁占着: {error}"
        );
        assert!(
            describe(browser_pid).is_some(),
            "冲突路径上绝不得动别人的浏览器进程 {browser_pid}"
        );
        assert!(path.exists(), "冲突时不得抹掉别人的记录");

        kill_tree(browser_pid, None).await;
        kill_tree(writer_pid, None).await;
        let _ = std::fs::remove_dir_all(profile.parent().expect("temp root"));
    }

    /// 起一个能活一阵的子进程,并采它的真实身份(当作"假浏览器")。
    #[cfg(windows)]
    async fn spawn_sleeper() -> (u32, String, u64) {
        use tokio::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let child = Command::new("cmd.exe")
            .args(["/C", "ping", "-n", "120", "127.0.0.1", ">", "nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id().expect("sleeper pid");
        // 刚 spawn 的几个 ms 内镜像路径可能还读不到,重试到拿齐为止。
        for _ in 0..50 {
            if let Some(probe) = describe(pid)
                && let (Some(exe), Some(created)) = (probe.exe, probe.created_ms)
            {
                // 故意 leak handle:不让 tokio 在子进程还活着时把它背身杀掉,
                // 回收路径必须由被测代码自己跑。
                std::mem::forget(child);
                return (pid, exe, created);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        std::mem::forget(child);
        panic!("无法探测子进程 {pid} 的身份");
    }

    fn temp_profile(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("denia-procctl-{name}-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        std::fs::create_dir_all(&profile).expect("create temp profile");
        profile
    }
}
