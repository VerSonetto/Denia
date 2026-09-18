//! 会话头部「用应用打开」:host 侧应用探测、图标提取与分离启动。
//!
//! 应用目录是编译期数据表:每个应用按平台声明一串定位器,按序尝试,第一个
//! 解析出真实存在可执行文件的胜出——探测结果永远是验证过的启动器,不是裸
//! 安装记录。整表惰性解析一次并缓存;某次启动发现可执行文件消失(ENOENT)
//! 时,只重解析该条并重试一次。
//!
//! 图标按平台提取:Windows 经 PowerShell `ExtractAssociatedIcon` 出 32px
//! PNG;macOS 读 bundle 的 `.icns` 经 `sips` 转 128px PNG;Linux 跟随
//! desktop entry 的 `Icon=` 走 hicolor 主题目录。提取失败缓存为空,路由答
//! 404,浏览器渲染通用占位方块。
//!
//! 绑定地址非回环(远程使用)时一律返回空目录:远程用户打不开宿主机上的
//! GUI 应用,探测也是白跑。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// 启动参数里携带工作区目录的占位符。
pub const PATH_TOKEN: &str = "{path}";

/// 解析期宿主命令(如 xcode-select)的单命令时限。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 图标提取的单命令时限(PowerShell 冷启动可达数秒)。
const ICON_TIMEOUT: Duration = Duration::from_secs(20);

/// 启动观察窗:窗内退出的子进程按退出码判定成败;窗内仍在跑视为成功,
/// 让它继续活下去。只约束 HTTP 响应的等待时长,不约束应用生命周期。
const LAUNCH_WATCH: Duration = Duration::from_millis(2000);

// ---------------------------------------------------------------------------
// 目录表
// ---------------------------------------------------------------------------

/// 目录表里的固定启动方式(static 数据,不含堆分配)。
#[derive(Debug, Clone, Copy)]
enum FixedLaunch {
    /// 分离 spawn 启动器,目录代入 `{path}` 占位符(无占位符则追加到末尾)。
    Argv {
        command: &'static str,
        args: &'static [&'static str],
    },
    /// 交给 OS shell 的 open 动作:文件管理器是目录的系统默认处理者,
    /// 直接 spawn explorer.exe 不能可靠把窗口带到前台。
    ShellOpen,
}

/// 一个平台的启动器来源链。
#[derive(Debug, Clone, Copy)]
struct PlatformSpec {
    /// 按序尝试;第一个解析出已验证启动器的胜出。
    locators: &'static [Locator],
    /// Linux 专属:图标的 XDG desktop entry id(`Icon=` 键的归属)。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    desktop_id: Option<&'static str>,
}

/// 单个定位器:每个变体都解析到本机真实持有之物——存在的 .app bundle、
/// 磁盘上的可执行文件或 PATH 解析结果。
#[derive(Debug, Clone, Copy)]
enum Locator {
    /// 随操作系统自带;图标路径直接信任(文件缺失在提取时表现为 404)。
    Fixed {
        launch: FixedLaunch,
        icon_path: &'static str,
    },
    /// 在已知 .app 目录(`/Applications`、`~/Applications`)找命名 bundle。
    App { fs_names: &'static [&'static str] },
    /// 跟随 `xcode-select -p` 定位 Xcode(Beta/改名安装也能找到)。
    Xcode,
    /// PATH 名解析(进程内 stat,不走 shell);Linux GUI 启动器可要求
    /// 存在桌面会话。
    Cli {
        name: &'static str,
        args: &'static [&'static str],
        requires_desktop: bool,
    },
    /// 候选文件路径取第一个存在者。
    File {
        candidates: &'static [&'static str],
        args: &'static [&'static str],
    },
    /// 版本化安装目录里取最新匹配(JetBrains on Windows)。
    Scan {
        root: &'static str,
        name_prefix: &'static str,
        relative_launcher: &'static str,
    },
    /// Windows `App Paths` 注册表键定位已注册可执行文件。
    AppPaths {
        exe: &'static str,
        args: &'static [&'static str],
    },
    /// Windows 卸载记录定位安装位置下的启动器;无相对启动器时退回
    /// `DisplayIcon` 指向的可执行文件。
    InstallRecord {
        display_name_prefix: &'static str,
        relative_launcher: Option<&'static str>,
        args: &'static [&'static str],
    },
    /// GitHub Desktop 版本化安装目录,经其打包 CLI 打开目录。
    GithubDesktop { root: &'static str },
    /// Linux XDG desktop entry 的 `Exec`/`TryExec` 定位。
    Desktop {
        desktop_id: &'static str,
        args: &'static [&'static str],
    },
}

/// 一个可启动应用及其各平台声明。
struct AppEntry {
    id: &'static str,
    darwin: Option<PlatformSpec>,
    win32: Option<PlatformSpec>,
    linux: Option<PlatformSpec>,
}

const fn spec(locators: &'static [Locator]) -> PlatformSpec {
    PlatformSpec {
        locators,
        desktop_id: None,
    }
}

const fn desktop_spec(desktop_id: &'static str, locators: &'static [Locator]) -> PlatformSpec {
    PlatformSpec {
        locators,
        desktop_id: Some(desktop_id),
    }
}

/// macOS spec:在已知应用目录里找命名 bundle。用宏而不是 const fn:
/// static 初始化里只有字面量数组能获得 'static 存续期。
macro_rules! mac_app {
    ([$name:literal $(, $rest:literal)*]) => {
        spec(&[Locator::App {
            fs_names: &[$name $(, $rest)*],
        }])
    };
}

const fn cli(name: &'static str, args: &'static [&'static str]) -> Locator {
    Locator::Cli {
        name,
        args,
        requires_desktop: false,
    }
}

const fn desktop_cli(name: &'static str, args: &'static [&'static str]) -> Locator {
    Locator::Cli {
        name,
        args,
        requires_desktop: true,
    }
}

const fn file(candidates: &'static [&'static str], args: &'static [&'static str]) -> Locator {
    Locator::File { candidates, args }
}

const fn app_paths(exe: &'static str, args: &'static [&'static str]) -> Locator {
    Locator::AppPaths { exe, args }
}

const fn install_record(
    display_name_prefix: &'static str,
    relative_launcher: Option<&'static str>,
    args: &'static [&'static str],
) -> Locator {
    Locator::InstallRecord {
        display_name_prefix,
        relative_launcher,
        args,
    }
}

const fn scan(
    root: &'static str,
    name_prefix: &'static str,
    relative_launcher: &'static str,
) -> Locator {
    Locator::Scan {
        root,
        name_prefix,
        relative_launcher,
    }
}

/// JetBrains 系产品条目:macOS 认 bundle 名(直装与 Toolbox 拼写),Windows
/// 取 `%ProgramFiles%\JetBrains` 最新版本目录或验证过的卸载记录,Linux 走
/// PATH 命令或 Toolbox shell 脚本。用宏而不是 const fn:static 初始化里
/// 只有字面量数组能获得 'static 存续期,const fn 的参数做不到。
macro_rules! jetbrains {
    (
        $id:literal, $product:literal, $linux_cli:literal,
        $win_launcher:literal, $toolbox_script:literal,
        $mac_name:literal $(, $mac_rest:literal)*
    ) => {
        AppEntry {
            id: $id,
            darwin: Some(mac_app!([$mac_name $(, $mac_rest)*])),
            win32: Some(spec(&[
                scan("${ProgramFiles}/JetBrains", $product, $win_launcher),
                install_record($product, Some($win_launcher), &[]),
            ])),
            linux: Some(spec(&[cli($linux_cli, &[]), file(&[$toolbox_script], &[])])),
        }
    };
}

/// 启动目录表,菜单顺序:文件管理器、编辑器与 IDE、Git GUI、终端。
/// Finder、Terminal、Explorer 随操作系统发行,定位器必然命中。
static CATALOG: &[AppEntry] = &[
    AppEntry {
        id: "finder",
        darwin: Some(spec(&[Locator::Fixed {
            launch: FixedLaunch::ShellOpen,
            icon_path: "/System/Library/CoreServices/Finder.app",
        }])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "explorer",
        darwin: None,
        win32: Some(spec(&[Locator::Fixed {
            launch: FixedLaunch::ShellOpen,
            icon_path: "${SystemRoot}/explorer.exe",
        }])),
        linux: None,
    },
    AppEntry {
        id: "filemanager",
        darwin: None,
        win32: None,
        linux: Some(spec(&[desktop_cli("xdg-open", &[])])),
    },
    AppEntry {
        id: "cursor",
        darwin: Some(mac_app!(["Cursor.app"])),
        win32: Some(spec(&[
            app_paths("Cursor.exe", &[]),
            install_record("Cursor", None, &[]),
            file(&["${LOCALAPPDATA}/Programs/cursor/Cursor.exe"], &[]),
        ])),
        linux: Some(spec(&[cli("cursor", &[])])),
    },
    AppEntry {
        id: "vscode",
        darwin: Some(mac_app!(["Visual Studio Code.app"])),
        win32: Some(spec(&[
            app_paths("Code.exe", &[]),
            install_record("Microsoft Visual Studio Code", Some("Code.exe"), &[]),
            file(
                &[
                    "${LOCALAPPDATA}/Programs/Microsoft VS Code/Code.exe",
                    "${ProgramFiles}/Microsoft VS Code/Code.exe",
                ],
                &[],
            ),
        ])),
        linux: Some(desktop_spec("code", &[cli("code", &[])])),
    },
    AppEntry {
        id: "vscodeinsiders",
        darwin: Some(mac_app!(["Visual Studio Code - Insiders.app"])),
        win32: Some(spec(&[
            app_paths("Code - Insiders.exe", &[]),
            install_record(
                "Microsoft Visual Studio Code Insiders",
                Some("Code - Insiders.exe"),
                &[],
            ),
            file(
                &["${LOCALAPPDATA}/Programs/Microsoft VS Code Insiders/Code - Insiders.exe"],
                &[],
            ),
        ])),
        linux: Some(desktop_spec("code-insiders", &[cli("code-insiders", &[])])),
    },
    AppEntry {
        id: "windsurf",
        darwin: Some(mac_app!(["Windsurf.app"])),
        win32: Some(spec(&[
            app_paths("Windsurf.exe", &[]),
            install_record("Windsurf", None, &[]),
            file(&["${LOCALAPPDATA}/Programs/Windsurf/Windsurf.exe"], &[]),
        ])),
        linux: Some(spec(&[cli("windsurf", &[])])),
    },
    AppEntry {
        id: "zed",
        darwin: Some(mac_app!(["Zed.app", "Zed Preview.app"])),
        win32: None,
        linux: Some(desktop_spec(
            "dev.zed.Zed",
            &[
                cli("zed", &[]),
                Locator::Desktop {
                    desktop_id: "dev.zed.Zed",
                    args: &[],
                },
            ],
        )),
    },
    AppEntry {
        id: "sublimetext",
        darwin: Some(mac_app!(["Sublime Text.app"])),
        win32: Some(spec(&[
            app_paths("sublime_text.exe", &[]),
            install_record("Sublime Text", None, &[]),
            file(&["${ProgramFiles}/Sublime Text/sublime_text.exe"], &[]),
        ])),
        linux: Some(desktop_spec("sublime_text", &[cli("subl", &[])])),
    },
    AppEntry {
        id: "xcode",
        darwin: Some(spec(&[Locator::Xcode])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "androidstudio",
        darwin: Some(mac_app!(["Android Studio.app"])),
        win32: Some(spec(&[
            install_record("Android Studio", Some("bin/studio64.exe"), &[]),
            file(&["${ProgramFiles}/Android/Android Studio/bin/studio64.exe"], &[]),
        ])),
        linux: Some(spec(&[
            cli("studio", &[]),
            file(
                &[
                    "~/.local/share/JetBrains/Toolbox/scripts/studio",
                    "/opt/android-studio/bin/studio.sh",
                ],
                &[],
            ),
        ])),
    },
    jetbrains!(
        "intellij",
        "IntelliJ IDEA",
        "idea",
        "bin/idea64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/idea",
        "IntelliJ IDEA.app",
        "IntelliJ IDEA Ultimate.app",
        "IntelliJ IDEA CE.app"
    ),
    jetbrains!(
        "pycharm",
        "PyCharm",
        "pycharm",
        "bin/pycharm64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/pycharm",
        "PyCharm.app",
        "PyCharm Professional.app",
        "PyCharm CE.app",
        "PyCharm Community.app"
    ),
    jetbrains!(
        "webstorm",
        "WebStorm",
        "webstorm",
        "bin/webstorm64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/webstorm",
        "WebStorm.app"
    ),
    jetbrains!(
        "phpstorm",
        "PhpStorm",
        "phpstorm",
        "bin/phpstorm64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/phpstorm",
        "PhpStorm.app"
    ),
    jetbrains!(
        "goland",
        "GoLand",
        "goland",
        "bin/goland64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/goland",
        "GoLand.app"
    ),
    jetbrains!(
        "rider",
        "Rider",
        "rider",
        "bin/rider64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/rider",
        "Rider.app",
        "JetBrains Rider.app"
    ),
    jetbrains!(
        "rustrover",
        "RustRover",
        "rustrover",
        "bin/rustrover64.exe",
        "~/.local/share/JetBrains/Toolbox/scripts/rustrover",
        "RustRover.app"
    ),
    AppEntry {
        id: "fork",
        darwin: Some(mac_app!(["Fork.app"])),
        win32: Some(spec(&[
            install_record("Fork", None, &[]),
            file(&["${LOCALAPPDATA}/Fork/Fork.exe"], &[]),
        ])),
        linux: None,
    },
    AppEntry {
        id: "sourcetree",
        darwin: Some(mac_app!(["Sourcetree.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "github",
        darwin: Some(mac_app!(["GitHub Desktop.app"])),
        win32: Some(spec(&[Locator::GithubDesktop {
            root: "${LOCALAPPDATA}/GitHubDesktop",
        }])),
        linux: None,
    },
    AppEntry {
        id: "tower",
        darwin: Some(mac_app!(["Tower.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "gitkraken",
        darwin: Some(mac_app!(["GitKraken.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "smartgit",
        darwin: Some(mac_app!(["SmartGit.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "sublimemerge",
        darwin: Some(mac_app!(["Sublime Merge.app"])),
        win32: Some(spec(&[
            app_paths("sublime_merge.exe", &[]),
            install_record("Sublime Merge", None, &[]),
            file(&["${ProgramFiles}/Sublime Merge/sublime_merge.exe"], &[]),
        ])),
        linux: Some(desktop_spec("sublime_merge", &[cli("smerge", &[])])),
    },
    AppEntry {
        id: "ghostty",
        darwin: Some(mac_app!(["Ghostty.app"])),
        win32: None,
        linux: Some(desktop_spec(
            "com.mitchellh.ghostty",
            &[
                cli("ghostty", &["--working-directory={path}"]),
                Locator::Desktop {
                    desktop_id: "com.mitchellh.ghostty",
                    args: &["--working-directory={path}"],
                },
            ],
        )),
    },
    AppEntry {
        id: "warp",
        darwin: Some(mac_app!(["Warp.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "iterm",
        darwin: Some(mac_app!(["iTerm.app"])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "kitty",
        darwin: Some(mac_app!(["kitty.app"])),
        win32: None,
        linux: Some(desktop_spec(
            "kitty",
            &[
                cli("kitty", &["--directory"]),
                Locator::Desktop {
                    desktop_id: "kitty",
                    args: &["--directory"],
                },
            ],
        )),
    },
    AppEntry {
        id: "terminal",
        darwin: Some(spec(&[Locator::Fixed {
            launch: FixedLaunch::Argv {
                command: "open",
                args: &["-a", "Terminal"],
            },
            icon_path: "/System/Applications/Utilities/Terminal.app",
        }])),
        win32: None,
        linux: None,
    },
    AppEntry {
        id: "windowsterminal",
        darwin: None,
        win32: Some(spec(&[cli("wt", &["-d"])])),
        linux: None,
    },
    AppEntry {
        id: "gitbash",
        darwin: None,
        win32: Some(spec(&[
            // Git for Windows 的卸载记录名形如 "Git version <x.y.z>";裸
            // "Git" 前缀会把 GitHub Desktop 一并匹配进来。
            install_record("Git version", Some("git-bash.exe"), &["--cd={path}"]),
            file(&["${ProgramFiles}/Git/git-bash.exe"], &["--cd={path}"]),
        ])),
        linux: None,
    },
    AppEntry {
        id: "gnometerminal",
        darwin: None,
        win32: None,
        linux: Some(desktop_spec(
            "org.gnome.Terminal",
            &[
                cli("gnome-terminal", &["--working-directory={path}"]),
                Locator::Desktop {
                    desktop_id: "org.gnome.Terminal",
                    args: &["--working-directory={path}"],
                },
            ],
        )),
    },
    AppEntry {
        id: "konsole",
        darwin: None,
        win32: None,
        linux: Some(desktop_spec(
            "org.kde.konsole",
            &[
                cli("konsole", &["--workdir"]),
                Locator::Desktop {
                    desktop_id: "org.kde.konsole",
                    args: &["--workdir"],
                },
            ],
        )),
    },
];

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 运行时启动方式(解析产物,持堆分配)。
#[derive(Debug, Clone)]
enum LaunchKind {
    Argv {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    ShellOpen,
}

/// 图标像素的来源。
#[derive(Debug, Clone)]
enum IconSource {
    /// macOS:bundle 目录(.icns 所在)。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    AppBundle(PathBuf),
    /// Windows:可执行文件自身(提取关联图标)。
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Executable(PathBuf),
}

/// 一条已验证的启动配置。
#[derive(Debug, Clone)]
struct ResolvedLaunch {
    launch: LaunchKind,
    fallback: Option<LaunchKind>,
    /// macOS/Windows 的图标来源;Linux 走 desktop entry,无此项。
    icon: Option<IconSource>,
}

/// Windows 卸载记录里与启动器推导相关的字段。
#[derive(Debug, Clone)]
struct InstallRecord {
    display_name: String,
    install_location: Option<String>,
    display_icon: Option<String>,
}

/// 一次解析的 Windows 注册表事实(直读注册表 API,不经 reg.exe 子进程,
/// 免去控制台代码页的编码陷阱)。
#[derive(Debug, Default)]
struct RegistryView {
    /// 小写 exe 名 → `App Paths` 默认值展开后的目标路径。
    app_paths: HashMap<String, String>,
    install_records: Vec<InstallRecord>,
}

/// 一次解析内共享的惰性注册表视图:用到时才读,且至多读一次。
struct RegistryOnce {
    #[cfg(windows)]
    view: tokio::sync::OnceCell<RegistryView>,
}

impl RegistryOnce {
    fn new() -> Self {
        Self {
            #[cfg(windows)]
            view: tokio::sync::OnceCell::new(),
        }
    }

    /// 平台不命中时返回 None,相关定位器直接判不可用。
    async fn view(&self) -> Option<&RegistryView> {
        #[cfg(windows)]
        {
            Some(
                self.view
                    .get_or_init(|| async {
                        tokio::task::spawn_blocking(read_registry_view)
                            .await
                            .unwrap_or_default()
                    })
                    .await,
            )
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

/// 直读 Windows 注册表:`App Paths` 表(HKCU 优先,per-user 安装遮蔽
/// 机器级安装)与三处卸载记录根(用户、64 位机器视图、32 位机器视图)。
/// 宽字符 API 不经子进程,值按 REG_SZ/REG_EXPAND_SZ 读取并展开 `%VAR%`。
#[cfg(windows)]
fn read_registry_view() -> RegistryView {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ,
    };

    const APP_PATHS_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths";
    const UNINSTALL_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
    const UNINSTALL_WOW64_SUBKEY: &str =
        "Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }

    fn open_key(parent: HKEY, path: &str) -> Option<HKEY> {
        let mut key: HKEY = unsafe { std::mem::zeroed() };
        let status =
            unsafe { RegOpenKeyExW(parent, wide(path).as_ptr(), 0, KEY_READ, &mut key) };
        (status == ERROR_SUCCESS).then_some(key)
    }

    /// 枚举直接子键名;超长名或枚举到头都终止。
    fn enum_subkeys(key: HKEY) -> Vec<String> {
        let mut names = Vec::new();
        let mut index = 0u32;
        loop {
            let mut buffer = [0u16; 256];
            let mut length = buffer.len() as u32;
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    buffer.as_mut_ptr(),
                    &mut length,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            names.push(from_wide(&buffer[..length as usize]));
            index += 1;
        }
        names
    }

    /// 读一个 REG_SZ/REG_EXPAND_SZ 值;`None` 值名即默认值。
    fn query_string(key: HKEY, name: Option<&str>) -> Option<String> {
        let name_wide = name.map(wide);
        let name_ptr = match &name_wide {
            Some(value) => value.as_ptr(),
            None => std::ptr::null(),
        };
        unsafe {
            let mut value_type = 0u32;
            let mut size = 0u32;
            if RegQueryValueExW(
                key,
                name_ptr,
                std::ptr::null(),
                &mut value_type,
                std::ptr::null_mut(),
                &mut size,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
                return None;
            }
            let mut buffer = vec![0u8; size as usize];
            if RegQueryValueExW(
                key,
                name_ptr,
                std::ptr::null(),
                std::ptr::null_mut(),
                buffer.as_mut_ptr(),
                &mut size,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            let pairs: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            Some(from_wide(&pairs))
        }
    }

    /// 打开子键读一个字符串值,读完即关。
    fn subkey_string(parent: HKEY, subkey: &str, name: Option<&str>) -> Option<String> {
        let key = open_key(parent, subkey)?;
        let value = query_string(key, name);
        unsafe { RegCloseKey(key) };
        value
    }

    let mut app_paths = HashMap::new();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let Some(root) = open_key(hive, APP_PATHS_SUBKEY) else {
            continue;
        };
        for subkey in enum_subkeys(root) {
            let lower = subkey.to_lowercase();
            if !lower.ends_with(".exe") || app_paths.contains_key(&lower) {
                continue;
            }
            if let Some(target) = subkey_string(root, &subkey, None) {
                // 默认值可能带引号与 %VAR%。
                if let Some(expanded) = expand_registry_value(target.trim().trim_matches('"')) {
                    app_paths.insert(lower.clone(), expanded);
                }
            }
        }
        unsafe { RegCloseKey(root) };
    }

    let mut install_records = Vec::new();
    let uninstall_roots = [
        (HKEY_CURRENT_USER, UNINSTALL_SUBKEY),
        (HKEY_LOCAL_MACHINE, UNINSTALL_SUBKEY),
        (HKEY_LOCAL_MACHINE, UNINSTALL_WOW64_SUBKEY),
    ];
    for (hive, subkey) in uninstall_roots {
        let Some(root) = open_key(hive, subkey) else {
            continue;
        };
        for entry in enum_subkeys(root) {
            let Some(display_name) = subkey_string(root, &entry, Some("DisplayName")) else {
                continue;
            };
            install_records.push(InstallRecord {
                display_name,
                install_location: subkey_string(root, &entry, Some("InstallLocation")),
                display_icon: subkey_string(root, &entry, Some("DisplayIcon")),
            });
        }
        unsafe { RegCloseKey(root) };
    }

    RegistryView {
        app_paths,
        install_records,
    }
}

fn spec_for(entry: &'static AppEntry) -> Option<&'static PlatformSpec> {
    match std::env::consts::OS {
        "macos" => entry.darwin.as_ref(),
        "windows" => entry.win32.as_ref(),
        "linux" => entry.linux.as_ref(),
        _ => None,
    }
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path).await.map(|m| m.is_dir()).unwrap_or(false)
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path).await.map(|m| m.is_file()).unwrap_or(false)
}

fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Linux GUI 启动器的前置条件:存在桌面会话。
fn has_desktop_session() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

/// 展开 `${VAR}` 与前导 `~/`;任一变量未设置即整体作废——残缺路径
/// 不是可信候选。
fn expand_candidate(template: &str) -> Option<String> {
    let mut missing = false;
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str("${");
            rest = after;
            continue;
        };
        match std::env::var(&after[..end]) {
            Ok(value) => out.push_str(&value),
            Err(_) => missing = true,
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    if missing {
        return None;
    }
    let expanded = if let Some(rest) = out.strip_prefix("~/") {
        home_dir().join(rest).to_string_lossy().into_owned()
    } else {
        out
    };
    Some(expanded)
}

/// 展开 Windows 注册表值里的 `%VAR%`;未设置的变量整体作废。
fn expand_registry_value(value: &str) -> Option<String> {
    let mut missing = false;
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push('%');
            rest = after;
            continue;
        };
        match std::env::var(&after[..end]) {
            Ok(value) => out.push_str(&value),
            Err(_) => missing = true,
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    (!missing).then_some(out)
}

/// 去掉 `DisplayIcon` 值尾部的 `,<索引>` 段(图标资源序号);先去引号
/// 再剥索引,两种写法都能处理。
fn strip_icon_index(icon: &str) -> &str {
    let trimmed = icon.trim().trim_matches('"');
    match trimmed.rfind(',') {
        Some(pos) => {
            let tail = &trimmed[pos + 1..];
            // 索引序号可能带负号。
            let digits = tail.strip_prefix('-').unwrap_or(tail);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                trimmed[..pos].trim()
            } else {
                trimmed
            }
        }
        None => trimmed,
    }
}

/// PATH 名解析的候选序列:原名优先,再逐个 PATHEXT 扩展名(与 cmd 的
/// 解析顺序一致,只 stat 不起 shell)。
fn path_candidates(name: &str) -> Vec<String> {
    if std::env::consts::OS != "windows" {
        return vec![name.to_string()];
    }
    let mut out = vec![name.to_string()];
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let lower = name.to_lowercase();
    for ext in exts.split(';').filter(|e| !e.is_empty()) {
        if lower.ends_with(&ext.to_lowercase()) {
            continue;
        }
        out.push(format!("{name}{ext}"));
    }
    out
}

/// 进程内 PATH 名解析:逐目录 stat,不走 shell。
async fn resolve_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        let path = PathBuf::from(name);
        return is_file(&path).await.then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for candidate in path_candidates(name) {
            let full = dir.join(&candidate);
            if is_file(&full).await {
                return Some(full);
            }
        }
    }
    None
}

/// 数字感知比较:数字段按数值比(升序),'2024.1.10' 排在 '2024.1.9'
/// 之后;纯字典序会把 10 排在 9 前面。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut ai, mut bi) = (0usize, 0usize);
    while ai < ab.len() && bi < bb.len() {
        match (ab[ai].is_ascii_digit(), bb[bi].is_ascii_digit()) {
            (true, true) => {
                let a_start = ai;
                while ai < ab.len() && ab[ai].is_ascii_digit() {
                    ai += 1;
                }
                let b_start = bi;
                while bi < bb.len() && bb[bi].is_ascii_digit() {
                    bi += 1;
                }
                let av = a[a_start..ai].parse::<u64>().unwrap_or(u64::MAX);
                let bv = b[b_start..bi].parse::<u64>().unwrap_or(u64::MAX);
                if av != bv {
                    return av.cmp(&bv);
                }
                // 数值相等时前导零多者靠后,保持确定顺序。
                let segment = a[a_start..ai].len().cmp(&b[b_start..bi].len());
                if segment != Ordering::Equal {
                    return segment;
                }
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {
                let a_start = ai;
                while ai < ab.len() && !ab[ai].is_ascii_digit() {
                    ai += 1;
                }
                let b_start = bi;
                while bi < bb.len() && !bb[bi].is_ascii_digit() {
                    bi += 1;
                }
                let ord = a[a_start..ai].cmp(&b[b_start..bi]);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    // 前缀关系:短者在前。
    (ab.len() - ai).cmp(&(bb.len() - bi))
}

/// XDG desktop entry 里解析器与图标路由关注的字段。
#[derive(Debug, Default, Clone)]
struct DesktopEntry {
    exec: Option<String>,
    try_exec: Option<String>,
    icon: Option<String>,
}

fn parse_desktop_entry(text: &str) -> DesktopEntry {
    let mut entry = DesktopEntry::default();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[Desktop Entry]";
            continue;
        }
        if !inside {
            continue;
        }
        let Some(sep) = trimmed.find('=') else { continue };
        let key = trimmed[..sep].trim();
        let value = trimmed[sep + 1..].trim();
        match key {
            "Exec" => entry.exec = Some(value.to_string()),
            "TryExec" => entry.try_exec = Some(value.to_string()),
            "Icon" => entry.icon = Some(value.to_string()),
            _ => {}
        }
    }
    entry
}

/// `Exec=` 的第一个 token:带引号取引号内,否则取到首个空白。
fn exec_command(exec: &str) -> Option<String> {
    let trimmed = exec.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    trimmed.split_whitespace().next().map(str::to_string)
}

/// XDG 数据目录(优先序):`XDG_DATA_HOME` 在前,后随 `XDG_DATA_DIRS`。
fn xdg_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"));
    let mut dirs = vec![data_home];
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs
}

async fn find_desktop_entry(desktop_id: &str) -> Option<DesktopEntry> {
    for dir in xdg_data_dirs() {
        let path = dir.join("applications").join(format!("{desktop_id}.desktop"));
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            return Some(parse_desktop_entry(&text));
        }
    }
    None
}

fn argv_launch(command: String, args: &[&str], icon: Option<IconSource>) -> ResolvedLaunch {
    ResolvedLaunch {
        launch: LaunchKind::Argv {
            command,
            args: args.iter().map(|s| s.to_string()).collect(),
            env: vec![],
        },
        fallback: None,
        icon,
    }
}

fn windows_exe_icon(path: &Path) -> Option<IconSource> {
    (std::env::consts::OS == "windows").then(|| IconSource::Executable(path.to_path_buf()))
}

/// 解析单个定位器,或 None(证明不了任何东西)。
async fn locate(locator: &Locator, registry: &RegistryOnce) -> Option<ResolvedLaunch> {
    match locator {
        Locator::Fixed { launch, icon_path } => {
            let launch = match *launch {
                FixedLaunch::Argv { command, args } => LaunchKind::Argv {
                    command: command.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    env: vec![],
                },
                FixedLaunch::ShellOpen => LaunchKind::ShellOpen,
            };
            let icon = expand_candidate(icon_path).map(|path| match std::env::consts::OS {
                "windows" => IconSource::Executable(PathBuf::from(path)),
                _ => IconSource::AppBundle(PathBuf::from(path)),
            });
            Some(ResolvedLaunch {
                launch,
                fallback: None,
                icon,
            })
        }
        Locator::App { fs_names } => {
            for root in [PathBuf::from("/Applications"), home_dir().join("Applications")] {
                for name in *fs_names {
                    let bundle = root.join(name);
                    if is_dir(&bundle).await {
                        return Some(ResolvedLaunch {
                            launch: LaunchKind::Argv {
                                command: "open".to_string(),
                                args: vec!["-a".to_string(), bundle.to_string_lossy().into_owned()],
                                env: vec![],
                            },
                            fallback: None,
                            icon: Some(IconSource::AppBundle(bundle)),
                        });
                    }
                }
            }
            None
        }
        Locator::Xcode => {
            let developer = output_command("xcode-select", &["-p".into()], PROBE_TIMEOUT).await?;
            // <bundle>/Contents/Developer → 上两层是 .app。
            let bundle = Path::new(developer.trim()).parent()?.parent()?;
            if !bundle.to_string_lossy().ends_with(".app") || !is_dir(bundle).await {
                return None;
            }
            Some(ResolvedLaunch {
                launch: LaunchKind::Argv {
                    command: "xed".to_string(),
                    args: vec![],
                    env: vec![],
                },
                fallback: Some(LaunchKind::Argv {
                    command: "open".to_string(),
                    args: vec!["-a".to_string(), bundle.to_string_lossy().into_owned()],
                    env: vec![],
                }),
                icon: Some(IconSource::AppBundle(bundle.to_path_buf())),
            })
        }
        Locator::Cli {
            name,
            args,
            requires_desktop,
        } => {
            if *requires_desktop && std::env::consts::OS == "linux" && !has_desktop_session() {
                return None;
            }
            let found = resolve_executable(name).await?;
            Some(argv_launch(
                found.to_string_lossy().into_owned(),
                args,
                windows_exe_icon(&found),
            ))
        }
        Locator::File { candidates, args } => {
            for candidate in *candidates {
                let Some(path) = expand_candidate(candidate) else { continue };
                let path = PathBuf::from(path);
                if is_file(&path).await {
                    return Some(argv_launch(
                        path.to_string_lossy().into_owned(),
                        args,
                        windows_exe_icon(&path),
                    ));
                }
            }
            None
        }
        Locator::Scan {
            root,
            name_prefix,
            relative_launcher,
        } => {
            let root = expand_candidate(root)?;
            let mut read_dir = match tokio::fs::read_dir(&root).await {
                Ok(read_dir) => read_dir,
                Err(_) => return None,
            };
            let mut versions = Vec::new();
            while let Ok(Some(item)) = read_dir.next_entry().await {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.starts_with(name_prefix) {
                    versions.push(name);
                }
            }
            versions.sort_by(|a, b| natural_cmp(b, a));
            for version in versions {
                let launcher = Path::new(&root).join(&version).join(relative_launcher);
                if is_file(&launcher).await {
                    return Some(argv_launch(
                        launcher.to_string_lossy().into_owned(),
                        &[],
                        windows_exe_icon(&launcher),
                    ));
                }
            }
            None
        }
        Locator::AppPaths { exe, args } => {
            let view = registry.view().await?;
            let target = view.app_paths.get(&exe.to_lowercase())?;
            let target = PathBuf::from(target);
            if !is_file(&target).await {
                return None;
            }
            Some(argv_launch(
                target.to_string_lossy().into_owned(),
                args,
                Some(IconSource::Executable(target)),
            ))
        }
        Locator::InstallRecord {
            display_name_prefix,
            relative_launcher,
            args,
        } => {
            let view = registry.view().await?;
            for record in &view.install_records {
                if !record.display_name.starts_with(display_name_prefix) {
                    continue;
                }
                if let Some(launcher) = record_launcher(record, *relative_launcher).await {
                    return Some(argv_launch(
                        launcher.to_string_lossy().into_owned(),
                        args,
                        Some(IconSource::Executable(launcher)),
                    ));
                }
            }
            None
        }
        Locator::GithubDesktop { root } => {
            let root = expand_candidate(root)?;
            let mut read_dir = match tokio::fs::read_dir(&root).await {
                Ok(read_dir) => read_dir,
                Err(_) => return None,
            };
            let mut versions = Vec::new();
            while let Ok(Some(item)) = read_dir.next_entry().await {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.starts_with("app-") {
                    versions.push(name);
                }
            }
            versions.sort_by(|a, b| natural_cmp(b, a));
            for version in versions {
                let directory = Path::new(&root).join(&version);
                let executable = directory.join("GitHubDesktop.exe");
                let cli = directory.join("resources").join("app").join("cli.js");
                if is_file(&executable).await && is_file(&cli).await {
                    return Some(ResolvedLaunch {
                        launch: LaunchKind::Argv {
                            command: executable.to_string_lossy().into_owned(),
                            args: vec![cli.to_string_lossy().into_owned(), "open".to_string()],
                            env: vec![("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())],
                        },
                        fallback: None,
                        icon: Some(IconSource::Executable(executable)),
                    });
                }
            }
            None
        }
        Locator::Desktop { desktop_id, args } => {
            let entry = find_desktop_entry(desktop_id).await?;
            let candidate = entry
                .try_exec
                .clone()
                .or_else(|| entry.exec.as_deref().and_then(exec_command))?;
            if candidate.is_empty() {
                return None;
            }
            let launcher = if Path::new(&candidate).is_absolute() {
                let path = PathBuf::from(&candidate);
                if !is_file(&path).await {
                    return None;
                }
                path
            } else {
                resolve_executable(&candidate).await?
            };
            Some(argv_launch(launcher.to_string_lossy().into_owned(), args, None))
        }
    }
}

/// 一条卸载记录证明的可执行文件:优先 `InstallLocation` 下的相对启动器,
/// 退回 `DisplayIcon` 指向的 exe。
async fn record_launcher(record: &InstallRecord, relative: Option<&str>) -> Option<PathBuf> {
    if let (Some(relative), Some(location)) = (relative, record.install_location.as_deref()) {
        let location = location.trim().trim_matches('"');
        if let Some(expanded) = expand_registry_value(location) {
            let candidate = Path::new(&expanded).join(relative);
            if is_file(&candidate).await {
                return Some(candidate);
            }
        }
    }
    if let Some(display_icon) = record.display_icon.as_deref() {
        let bare = strip_icon_index(display_icon).trim_matches('"');
        if let Some(expanded) = expand_registry_value(bare) {
            if expanded.to_lowercase().ends_with(".exe") {
                let path = PathBuf::from(expanded);
                if is_file(&path).await {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// 解析一个目录条目:本平台定位器按序尝试,第一个胜出。
async fn resolve_entry(entry: &'static AppEntry) -> Option<ResolvedLaunch> {
    let registry = RegistryOnce::new();
    let spec = spec_for(entry)?;
    for locator in spec.locators {
        if let Some(found) = locate(locator, &registry).await {
            return Some(found);
        }
    }
    None
}

/// 整表解析一次,保持菜单顺序。
async fn resolve_all() -> HashMap<String, ResolvedLaunch> {
    let mut map = HashMap::new();
    for entry in CATALOG {
        if let Some(resolved) = resolve_entry(entry).await {
            map.insert(entry.id.to_string(), resolved);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchOutcome {
    Launched,
    /// 启动器可执行文件消失:解析已过期。
    Missing,
    Failed,
}

/// 占位符代入:任一参数携带 `{path}` 则全量替换,否则把目录追加到末尾。
fn substitute_path(args: &[String], path: &Path) -> Vec<String> {
    let directory = path.to_string_lossy().into_owned();
    if args.iter().any(|arg| arg.contains(PATH_TOKEN)) {
        args.iter().map(|arg| arg.replace(PATH_TOKEN, &directory)).collect()
    } else {
        args.iter().cloned().chain(std::iter::once(directory)).collect()
    }
}

/// 启动的 GUI 应用不继承宿主进程的凭据类环境变量。
fn scrub_credentials(command: &mut tokio::process::Command) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.contains("KEY")
            || upper.contains("SECRET")
            || upper.contains("TOKEN")
            || upper.contains("PASSWORD")
        {
            command.env_remove(&name);
        }
    }
}

/// 一次分离 GUI 启动:无 stdio 管道、脱离本进程组/控制台,启动成败与
/// 进程存续解耦——kitty、JetBrains 这类启动器会常驻整个窗口生命周期,
/// 观察窗只拦截"立刻就失败"的启动器。
async fn run_launch(launch: &LaunchKind, path: &Path) -> LaunchOutcome {
    match launch {
        LaunchKind::ShellOpen => shell_open(path).await,
        LaunchKind::Argv { command, args, env } => {
            let mut child_command = tokio::process::Command::new(command);
            child_command.args(substitute_path(args, path));
            scrub_credentials(&mut child_command);
            for (key, value) in env {
                child_command.env(key, value);
            }
            child_command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use windows_sys::Win32::System::Threading::{
                    CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
                };
                child_command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let _ = child_command.process_group(0);
            }
            match child_command.spawn() {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => LaunchOutcome::Missing,
                Err(_) => LaunchOutcome::Failed,
                Ok(mut child) => match tokio::time::timeout(LAUNCH_WATCH, child.wait()).await {
                    Ok(Ok(status)) if status.success() => LaunchOutcome::Launched,
                    Ok(_) => LaunchOutcome::Failed,
                    // 观察窗关闭时仍在运行:计数成功,子进程继续跑
                    // (tokio 会在其退出后收割,不留僵尸)。
                    Err(_) => LaunchOutcome::Launched,
                },
            }
        }
    }
}

#[cfg(windows)]
async fn shell_open(path: &Path) -> LaunchOutcome {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // ShellExecuteW 是快速同步调用,挪进阻塞线程池。
    let mut file: Vec<u16> = path.as_os_str().encode_wide().collect();
    file.push(0);
    let raised = tokio::task::spawn_blocking(move || unsafe {
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let hinstance = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        // 传统约定:返回值 ≤ 32 是 SE_ERR 错误码。
        hinstance as isize > 32
    })
    .await
    .unwrap_or(false);
    if raised {
        LaunchOutcome::Launched
    } else {
        LaunchOutcome::Failed
    }
}

#[cfg(not(windows))]
async fn shell_open(path: &Path) -> LaunchOutcome {
    match std::env::consts::OS {
        "macos" => {
            run_launch(
                &LaunchKind::Argv {
                    command: "/usr/bin/open".to_string(),
                    args: vec![],
                    env: vec![],
                },
                path,
            )
            .await
        }
        "linux" => {
            if !has_desktop_session() {
                return LaunchOutcome::Failed;
            }
            run_launch(
                &LaunchKind::Argv {
                    command: "xdg-open".to_string(),
                    args: vec![],
                    env: vec![],
                },
                path,
            )
            .await
        }
        _ => LaunchOutcome::Failed,
    }
}

/// 主启动器失败且存在后备时再试后备;任一尝试的可执行文件消失都判
/// Missing,让调用方重解析一次。
async fn launch_resolved(resolved: &ResolvedLaunch, path: &Path) -> LaunchOutcome {
    let primary = run_launch(&resolved.launch, path).await;
    if primary == LaunchOutcome::Launched || resolved.fallback.is_none() {
        return primary;
    }
    let fallback = run_launch(resolved.fallback.as_ref().unwrap(), path).await;
    if fallback == LaunchOutcome::Launched {
        LaunchOutcome::Launched
    } else if primary == LaunchOutcome::Missing || fallback == LaunchOutcome::Missing {
        LaunchOutcome::Missing
    } else {
        LaunchOutcome::Failed
    }
}

// ---------------------------------------------------------------------------
// 图标
// ---------------------------------------------------------------------------

/// 一个应用的图标:字节与媒体类型。
#[derive(Debug, Clone)]
pub struct Icon {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

/// 跑一条有输出超时的宿主命令;失败(起不来/非零退出/超时)只有一个
/// 含义——拿不到结果。
async fn output_command(
    program: &str,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // 不为宿主命令弹控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = tokio::time::timeout(timeout, command.output()).await.ok()?.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn work_dir() -> PathBuf {
    std::env::temp_dir().join(format!("denia-open-in-app-{}", uuid::Uuid::new_v4()))
}

/// macOS:bundle 的 `.icns` 转 128px PNG。优先 Info.plist 声明的
/// `CFBundleIconFile`(可省略 .icns 后缀),退回 Resources 下首个 .icns。
#[cfg(target_os = "macos")]
async fn macos_bundle_icon(bundle: &Path) -> Option<Vec<u8>> {
    let resources = bundle.join("Contents").join("Resources");
    let plist = bundle.join("Contents").join("Info.plist");
    let mut icon_file: Option<String> = None;
    let args = vec![
        "-convert".into(),
        "json".into(),
        "-o".into(),
        "-".into(),
        plist.into_os_string(),
    ];
    if let Some(json) = output_command("/usr/bin/plutil", &args, ICON_TIMEOUT).await {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(name) = value.get("CFBundleIconFile").and_then(|v| v.as_str()) {
                icon_file = Some(if name.ends_with(".icns") {
                    name.to_string()
                } else {
                    format!("{name}.icns")
                });
            }
        }
    }
    if icon_file.is_none() {
        let mut found = None;
        if let Ok(mut entries) = tokio::fs::read_dir(&resources).await {
            while let Ok(Some(item)) = entries.next_entry().await {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.ends_with(".icns") {
                    found = Some(name);
                    break;
                }
            }
        }
        icon_file = found;
    }
    let icns = resources.join(icon_file?);
    if !is_file(&icns).await {
        return None;
    }
    let work = work_dir();
    tokio::fs::create_dir_all(&work).await.ok()?;
    let png = work.join("icon.png");
    let args = vec![
        "-s".into(),
        "format".into(),
        "png".into(),
        "-Z".into(),
        "128".into(),
        icns.clone().into_os_string(),
        png.clone().into_os_string(),
    ];
    let converted = output_command("/usr/bin/sips", &args, ICON_TIMEOUT)
        .await
        .is_some();
    let bytes = if converted {
        tokio::fs::read(&png).await.ok()
    } else {
        None
    };
    let _ = tokio::fs::remove_dir_all(&work).await;
    bytes
}

/// Windows:可执行文件的关联图标,32px PNG(`ExtractAssociatedIcon` 无
/// 原生扩展时的上限)。`-File` + 位置参数把路径挡在命令行解析面之外。
#[cfg(windows)]
const EXTRACT_ICON_PS1: &str = "\
param([string]$Source, [string]$Target)
$ErrorActionPreference = \"Stop\"
Add-Type -AssemblyName System.Drawing
$icon = [System.Drawing.Icon]::ExtractAssociatedIcon($Source)
if ($null -eq $icon) { exit 1 }
$bitmap = $icon.ToBitmap()
$bitmap.Save($Target, [System.Drawing.Imaging.ImageFormat]::Png)
";

#[cfg(windows)]
async fn windows_exe_icon_png(executable: &Path) -> Option<Vec<u8>> {
    let work = work_dir();
    tokio::fs::create_dir_all(&work).await.ok()?;
    let script = work.join("extract-icon.ps1");
    let png = work.join("icon.png");
    tokio::fs::write(&script, EXTRACT_ICON_PS1).await.ok()?;
    let args = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-File".into(),
        script.clone().into_os_string(),
        executable.as_os_str().to_os_string(),
        png.clone().into_os_string(),
    ];
    let ran = output_command("powershell.exe", &args, ICON_TIMEOUT).await;
    let bytes = if ran.is_some() {
        tokio::fs::read(&png).await.ok()
    } else {
        None
    };
    // 清理失败不致命:临时目录由系统兜底。
    let _ = tokio::fs::remove_dir_all(&work).await;
    bytes
}

/// 按钮渲染尺寸只有 15-18 CSS px,主题尺寸从大到小取第一个命中。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const HICOLOR_SIZES: [&str; 6] = ["512x512", "256x256", "128x128", "64x64", "48x48", "32x32"];

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
async fn read_icon_file(path: &Path) -> Option<Icon> {
    let content_type = match path.extension()?.to_str()? {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(Icon { content_type, bytes })
}

/// Linux:desktop entry 的 `Icon=` 键——绝对路径直读,否则走 hicolor
/// 主题与 pixmaps 目录(hicolor 是所有主题的回落基线)。
#[cfg(target_os = "linux")]
async fn linux_desktop_icon(desktop_id: &str) -> Option<Icon> {
    let entry = find_desktop_entry(desktop_id).await?;
    let name = entry.icon?;
    if name.is_empty() {
        return None;
    }
    if Path::new(&name).is_absolute() {
        return read_icon_file(Path::new(&name)).await;
    }
    for dir in xdg_data_dirs() {
        for size in HICOLOR_SIZES {
            for ext in ["png", "svg"] {
                if let Some(icon) = read_icon_file(
                    &dir.join("icons/hicolor")
                        .join(size)
                        .join("apps")
                        .join(format!("{name}.{ext}")),
                )
                .await
                {
                    return Some(icon);
                }
            }
        }
        if let Some(icon) =
            read_icon_file(&dir.join("icons/hicolor/scalable/apps").join(format!("{name}.svg")))
                .await
        {
            return Some(icon);
        }
        for ext in ["png", "svg"] {
            if let Some(icon) =
                read_icon_file(&dir.join("pixmaps").join(format!("{name}.{ext}"))).await
            {
                return Some(icon);
            }
        }
    }
    None
}

async fn extract_icon(entry: &'static AppEntry, resolved: ResolvedLaunch) -> Option<Icon> {
    match std::env::consts::OS {
        "linux" => {
            #[cfg(target_os = "linux")]
            {
                let desktop_id = spec_for(entry)?.desktop_id?;
                linux_desktop_icon(desktop_id).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = entry;
                None
            }
        }
        "macos" => {
            #[cfg(target_os = "macos")]
            {
                match resolved.icon? {
                    IconSource::AppBundle(bundle) => macos_bundle_icon(&bundle)
                        .await
                        .map(|bytes| Icon { content_type: "image/png", bytes }),
                    IconSource::Executable(_) => None,
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = resolved;
                None
            }
        }
        "windows" => {
            #[cfg(windows)]
            {
                match resolved.icon? {
                    IconSource::Executable(executable) => windows_exe_icon_png(&executable)
                        .await
                        .map(|bytes| Icon { content_type: "image/png", bytes }),
                    IconSource::AppBundle(_) => None,
                }
            }
            #[cfg(not(windows))]
            {
                let _ = resolved;
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 启动结果给路由层的三种结局。
pub enum OpenOutcome {
    Launched,
    /// 应用 id 不在目录里,或本机未解析到。
    UnknownApp,
    Failed,
}

/// 「用应用打开」的进程级状态:目录解析一次,图标按需提取并缓存
/// (失败也缓存,404 不会反复触发提取)。
pub struct OpenInAppState {
    /// 绑定地址非回环(远程使用):宿主机 GUI 对远程用户无意义。
    remote: bool,
    resolutions: tokio::sync::OnceCell<std::sync::RwLock<HashMap<String, ResolvedLaunch>>>,
    icons: std::sync::Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Option<Icon>>>>>,
}

impl OpenInAppState {
    pub fn new(remote: bool) -> Self {
        Self {
            remote,
            resolutions: tokio::sync::OnceCell::new(),
            icons: std::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn resolutions(&self) -> &std::sync::RwLock<HashMap<String, ResolvedLaunch>> {
        let remote = self.remote;
        self.resolutions
            .get_or_init(|| async move {
                let map = if remote { HashMap::new() } else { resolve_all().await };
                std::sync::RwLock::new(map)
            })
            .await
    }

    /// 本机已解析到的应用 id,保持目录(菜单)顺序。
    pub async fn available_ids(&self) -> Vec<&'static str> {
        let lock = self.resolutions().await;
        let map = lock.read().unwrap_or_else(|p| p.into_inner());
        CATALOG
            .iter()
            .filter(|entry| map.contains_key(entry.id))
            .map(|entry| entry.id)
            .collect()
    }

    async fn resolved_for(&self, id: &str) -> Option<ResolvedLaunch> {
        let lock = self.resolutions().await;
        let map = lock.read().unwrap_or_else(|p| p.into_inner());
        map.get(id).cloned()
    }

    /// 重解析一个条目:消失的条目移出目录,图标缓存一并作废。
    async fn refresh_entry(&self, id: &str) -> Option<ResolvedLaunch> {
        let entry = CATALOG.iter().find(|entry| entry.id == id)?;
        let fresh = resolve_entry(entry).await;
        {
            let lock = self.resolutions().await;
            let mut map = lock.write().unwrap_or_else(|p| p.into_inner());
            match &fresh {
                Some(resolved) => {
                    map.insert(id.to_string(), resolved.clone());
                }
                None => {
                    map.remove(id);
                }
            }
        }
        self.icons
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        fresh
    }

    /// 一个应用的图标,按需提取,含失败在内的结果都缓存。
    pub async fn icon(&self, id: &str) -> Option<Icon> {
        let entry: &'static AppEntry = CATALOG.iter().find(|entry| entry.id == id)?;
        let resolved = self.resolved_for(id).await?;
        let cell = {
            let mut cache = self.icons.lock().unwrap_or_else(|p| p.into_inner());
            cache
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };
        cell.get_or_init(|| extract_icon(entry, resolved)).await.clone()
    }

    /// 用一个已解析应用打开目录;启动器消失(自探测后被卸载)时重解析
    /// 该条并重试一次。
    pub async fn launch(&self, app: &str, path: &Path) -> OpenOutcome {
        if CATALOG.iter().all(|entry| entry.id != app) {
            return OpenOutcome::UnknownApp;
        }
        let Some(resolved) = self.resolved_for(app).await else {
            return OpenOutcome::UnknownApp;
        };
        let mut outcome = launch_resolved(&resolved, path).await;
        if outcome == LaunchOutcome::Missing {
            outcome = match self.refresh_entry(app).await {
                Some(fresh) => launch_resolved(&fresh, path).await,
                None => LaunchOutcome::Failed,
            };
        }
        if outcome == LaunchOutcome::Launched {
            OpenOutcome::Launched
        } else {
            OpenOutcome::Failed
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_candidate_substitutes_variable() {
        // SAFETY:测试专用变量名,单线程测试进程内无并发读取。
        unsafe { std::env::set_var("DENIA_OIA_TEST_ROOT", "C:/Program Files") };
        let expanded = expand_candidate("${DENIA_OIA_TEST_ROOT}/Sublime Text/sublime_text.exe")
            .expect("变量已设置");
        assert_eq!(expanded, "C:/Program Files/Sublime Text/sublime_text.exe");
        unsafe { std::env::remove_var("DENIA_OIA_TEST_ROOT") };
    }

    #[test]
    fn expand_candidate_fails_on_unset_variable() {
        unsafe { std::env::remove_var("DENIA_OIA_TEST_MISSING") };
        assert!(expand_candidate("${DENIA_OIA_TEST_MISSING}/x.exe").is_none());
    }

    #[test]
    fn expand_registry_value_substitutes_percent_vars() {
        // SAFETY:测试专用变量名,单线程测试进程内无并发读取。
        unsafe { std::env::set_var("DENIA_OIA_TEST_DIR", "D:\\apps") };
        assert_eq!(
            expand_registry_value("%DENIA_OIA_TEST_DIR%\\bin\\x.exe").as_deref(),
            Some("D:\\apps\\bin\\x.exe")
        );
        unsafe { std::env::remove_var("DENIA_OIA_TEST_DIR") };
    }

    #[test]
    fn substitute_path_replaces_token_or_appends() {
        let with_token = vec![format!("--cd={PATH_TOKEN}")];
        assert_eq!(
            substitute_path(&with_token, Path::new("D:\\ws")),
            vec!["--cd=D:\\ws"]
        );
        let without_token = vec!["-d".to_string()];
        assert_eq!(
            substitute_path(&without_token, Path::new("D:\\ws")),
            vec!["-d", "D:\\ws"]
        );
    }

    #[test]
    fn strip_icon_index_removes_trailing_resource_number() {
        assert_eq!(strip_icon_index("\"C:\\a\\x.exe,0\""), "C:\\a\\x.exe");
        assert_eq!(strip_icon_index("C:\\a\\x.exe,-1"), "C:\\a\\x.exe");
        assert_eq!(strip_icon_index("C:\\a,x\\x.exe"), "C:\\a,x\\x.exe");
    }

    #[test]
    fn natural_cmp_orders_versions_numerically() {
        // 数字段按数值升序;'I' 的字节序在 'a' 之前,跨前缀的比较只保证
        // 确定性,数值语义在同一前缀内生效。
        let mut names = vec![
            "IntelliJ IDEA 2024.1.9",
            "IntelliJ IDEA 2024.1.10",
            "app-2.14.1",
            "app-2.9.0",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                "IntelliJ IDEA 2024.1.9",
                "IntelliJ IDEA 2024.1.10",
                "app-2.9.0",
                "app-2.14.1"
            ]
        );
    }

    #[test]
    fn parse_desktop_entry_reads_recognized_keys_only() {
        let entry = parse_desktop_entry(
            "[Desktop Action new-window]\nExec=ignored --flag\n[Desktop Entry]\nName=Code\nExec=\"'/opt/code/code'\" %F\nTryExec=code\nIcon=visual-studio-code\n",
        );
        assert_eq!(entry.try_exec.as_deref(), Some("code"));
        assert_eq!(entry.icon.as_deref(), Some("visual-studio-code"));
        assert!(entry.exec.as_deref().unwrap().starts_with("\"'/opt/code/code'\""));
    }

    #[test]
    fn exec_command_takes_quoted_or_bare_first_token() {
        assert_eq!(
            exec_command("\"/opt/my app/code\" %F").as_deref(),
            Some("/opt/my app/code")
        );
        assert_eq!(exec_command("code --new-window").as_deref(), Some("code"));
        assert_eq!(exec_command("   "), None);
    }

    #[cfg(windows)]
    #[test]
    fn path_candidates_appends_pathext_in_order() {
        // SAFETY:测试专用变量名,单线程测试进程内无并发读取。
        unsafe { std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD") };
        assert_eq!(
            path_candidates("wt"),
            vec!["wt", "wt.COM", "wt.EXE", "wt.BAT", "wt.CMD"]
        );
        // 已带 .exe 后缀的名字跳过 .EXE,其余扩展名照试。
        assert_eq!(
            path_candidates("git.exe"),
            vec!["git.exe", "git.exe.COM", "git.exe.BAT", "git.exe.CMD"]
        );
    }

    #[test]
    fn catalog_ids_are_unique_and_menu_order_is_stable() {
        let ids: Vec<&str> = CATALOG.iter().map(|entry| entry.id).collect();
        let expected_head = ["finder", "explorer", "filemanager", "cursor", "vscode"];
        assert_eq!(ids[..expected_head.len()], expected_head);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "目录 id 必须唯一");
    }

    #[test]
    fn linux_specs_with_desktop_locators_own_an_icon_entry() {
        for entry in CATALOG {
            let Some(spec) = entry.linux.as_ref() else { continue };
            let has_desktop_locator = spec
                .locators
                .iter()
                .any(|locator| matches!(locator, Locator::Desktop { .. }));
            if has_desktop_locator {
                assert!(
                    spec.desktop_id.is_some(),
                    "app '{}' 的 desktop 定位器缺少图标归属 entry",
                    entry.id
                );
            }
        }
    }
}
