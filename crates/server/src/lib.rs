//! denia 服务端:axum HTTP API + SSE push + 控制台托管。
//!
//! 拆成 lib + bin 是为了让两个宿主共用同一份启动路径:
//!
//! - `denia`(bin):纯服务宿主,控制台由浏览器访问;
//! - `denia-desktop`:Tauri 壳,进程内起同一份服务,窗口加载本机地址。
//!
//! 路由装配、来源标注、优雅停机只有一份(见 [`host`])—— 两边各写一套的话,
//! 「本机直通 / 远程门 / 静态资源 fallback」的顺序迟早会漂移,桌面端与 Web 端
//! 的行为也就跟着分叉。

pub mod agent_presets;
pub mod agent_runtime;
pub mod api;
pub mod error;
pub mod event_pulse;
pub mod file_history;
pub mod host;
pub mod jobs;
pub mod mcp_runtime;
pub mod mcp_settings;
pub mod native_folder_picker;
pub mod open_in_app;
pub mod preset_tool;
pub mod project_memory;
pub mod remote;
pub mod session_title;
pub mod skills;
pub mod state;
pub mod system_prompt_store;
pub mod web_assets;
pub mod workspace;
pub mod workspace_instructions;
