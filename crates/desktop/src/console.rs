//! 给桌面端一个"看不见但存在"的控制台。
//!
//! ## 为什么需要
//!
//! 桌面端是 GUI 子系统程序(`windows_subsystem = "windows"`),自己没有控制台。
//! Windows 的规则是:控制台子进程默认**继承父进程的控制台**;父进程没有控制台
//! 时,系统给它**新建一个** —— 那个新控制台就是一个黑框窗口。
//!
//! 于是桌面端里每一次 `git`、每一次 `bash`、每个 MCP 服务器、cloudflared、
//! taskkill 都会在用户眼前闪一个黑窗。启动时那一个是 MCP 服务器(`npx`),
//! 审查面板里刷屏的那一片是反复调 `git`。
//!
//! `denia.exe` 从来看不到这些闪窗,因为它是控制台程序,子进程直接继承它的
//! 控制台。这不是"桌面端特有 bug",而是换了个子系统之后继承链断了的后果。
//!
//! ## 怎么修
//!
//! 让桌面端自己持有一个控制台,子进程照常继承它 —— 只是那个控制台的窗口是
//! 隐藏的。这样**所有 spawn 点都不必各自记得加 `CREATE_NO_WINDOW`**:漏一个
//! 就是一个闪窗,而漏掉是迟早的事(git、MCP 这两处就是这么漏的)。
//!
//! 顺序上先试 [`AttachConsole`]:从终端启动时接上调用者的控制台,日志照常
//! 打在终端里(否则用户会以为程序没输出)。接不上(双击启动)才自建并隐藏。
//!
//! ## 与 `--pick-folder` 子进程的关系
//!
//! 目录选择器子进程**绝不能**走这里:它的 stdout 是父进程给的管道,JSON 结果
//! 靠它回传;一旦被 [`reopen_std_handles`] 改指到控制台,父进程就永远读不到
//! 结果(表现为点"选择工作区目录"没反应)。所以 `main` 里 `--pick-folder` 的
//! 判断必须排在 `adopt()` **之前**。
//!
//! 子进程自己不需要控制台:它由父进程以 `CREATE_NO_WINDOW` 启动,不会新建
//! 黑框;rfd 的对话框是窗口不是控制台,照样是它的第一个窗口(Win32 前台锁
//! 要求如此,见 `denia_server::native_folder_picker`)。

#[cfg(windows)]
pub fn adopt() {
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, GetConsoleWindow,
    };

    unsafe {
        // 已经有控制台(debug 构建是控制台子系统),不动它。
        if !GetConsoleWindow().is_null() {
            return;
        }
        // 从终端里启动:接上那个终端,日志留在用户看得见的地方。
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            reopen_std_handles();
            return;
        }
        // 双击启动:自建一个并立刻藏掉。子进程会继承它,因此不会再弹黑框。
        if AllocConsole() != 0 {
            hide_new_window();
        }
    }
}

/// 非 Windows 平台没有这个问题(也没有这个 API)。
#[cfg(not(windows))]
pub fn adopt() {}

/// 接上控制台后把标准句柄指过去。
///
/// GUI 子系统进程的标准句柄默认是空的,`AttachConsole` **不会**替我们设置
/// 它们 —— 不补这一步,`println!` 与 tracing 仍然什么都写不出来,用户会以为
/// 程序没输出。
///
/// 但**只接管空句柄**:调用者可能已经把 stdout/stderr 重定向到了文件或管道
/// (`denia-desktop.exe > log.txt`、`--pick-folder` 的 JSON 回传都靠它),那是
/// 有意的,不该被我们改回控制台。
///
/// 用 `std::fs` 打开 `CONOUT$`/`CONIN$` 而不是自己调 `CreateFileW`:`File`
/// 本身就持有那个句柄(Windows 下 `AsRawHandle` 拿到的就是 `HANDLE`),不必为了
/// 一个打开动作把 `Win32_Security` 整套 feature 拖进来。
#[cfg(windows)]
fn reopen_std_handles() {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    // 这两个 `File` 必须活得比函数长:关掉它们等于关掉句柄,标准流又变成
    // 悬空的。泄漏一份是有意的(进程活多久它就用多久)。
    if std_handle_is_empty(STD_OUTPUT_HANDLE) {
        if let Ok(output) = OpenOptions::new().write(true).open("CONOUT$") {
            let handle = output.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            unsafe {
                SetStdHandle(STD_OUTPUT_HANDLE, handle);
                SetStdHandle(STD_ERROR_HANDLE, handle);
            }
            std::mem::forget(output);
        }
    }
    if std_handle_is_empty(STD_INPUT_HANDLE)
        && let Ok(input) = OpenOptions::new().read(true).open("CONIN$")
    {
        let handle = input.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        unsafe {
            SetStdHandle(STD_INPUT_HANDLE, handle);
        }
        std::mem::forget(input);
    }
}

/// 该标准句柄是不是空的(没有控制台、也没被重定向)。
#[cfg(windows)]
fn std_handle_is_empty(which: u32) -> bool {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::GetStdHandle;

    let handle: HANDLE = unsafe { GetStdHandle(which) };
    handle.is_null() || handle == INVALID_HANDLE_VALUE
}

/// 藏掉 `AllocConsole` 刚建出来的那个窗口。
///
/// 带短重试:窗口通常已经建好,但偶发时序下可能还没出来,漏掉就是一个用户
/// 看得见的黑框。
#[cfg(windows)]
fn hide_new_window() {
    use std::time::Duration;
    use windows_sys::Win32::System::Console::GetConsoleWindow;

    for _ in 0..40 {
        let window = unsafe { GetConsoleWindow() };
        if !window.is_null() {
            hide_window(window);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(windows)]
fn hide_window(window: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    if window.is_null() {
        return;
    }
    unsafe {
        ShowWindow(window, SW_HIDE);
    }
}
