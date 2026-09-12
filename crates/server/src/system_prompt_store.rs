//! 自定义系统提示词:`<home>/SYSTEM.md` 持久化 + 热加载。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use denia_system_prompt::SystemPrompt;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::broadcast;

use crate::state::ServerEvent;

const FILE_NAME: &str = "SYSTEM.md";

/// 全局系统提示词状态:读路径无锁,写/文件变更原子替换。
pub struct SystemPromptState {
    current: Arc<ArcSwap<SystemPrompt>>,
    home: PathBuf,
    browser_hub: Option<denia_tools::BrowserHub>,
}

impl SystemPromptState {
    /// 启动时从磁盘加载;空或缺失则沿用出厂 persona。
    pub fn load(home: &Path, browser_hub: Option<denia_tools::BrowserHub>) -> Self {
        let prompt = build_prompt(read_file_text(home), browser_hub.clone());
        Self {
            current: Arc::new(ArcSwap::from_pointee(prompt)),
            home: home.to_path_buf(),
            browser_hub,
        }
    }

    pub fn handle(&self) -> Arc<ArcSwap<SystemPrompt>> {
        self.current.clone()
    }

    pub fn file_path(&self) -> PathBuf {
        self.home.join(FILE_NAME)
    }

    /// 当前文件全文与来源标记。
    pub fn text_and_source(&self) -> (String, &'static str) {
        match std::fs::read_to_string(self.file_path()) {
            Ok(text) if !text.trim().is_empty() => (text, "file"),
            _ => (String::new(), "default"),
        }
    }

    /// 写入 `SYSTEM.md` 并热替换内存中的 `SystemPrompt`。
    pub fn write(&self, text: &str) -> std::io::Result<()> {
        let path = self.file_path();
        if text.trim().is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
        }
        self.reload();
        Ok(())
    }

    /// 删除自定义文件并恢复出厂 persona。
    pub fn reset(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_file(self.file_path());
        self.reload();
        Ok(())
    }

    fn reload(&self) {
        let prompt = build_prompt(read_file_text(&self.home), self.browser_hub.clone());
        self.current.store(Arc::new(prompt));
    }

    fn is_system_md(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == FILE_NAME)
    }

    /// 监听 `SYSTEM.md` 变更,debounce 后重载并广播。
    pub fn spawn_watcher(self: &Arc<Self>, events: broadcast::Sender<ServerEvent>) {
        let home = self.home.clone();
        let store = Arc::clone(self);
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match RecommendedWatcher::new(
                move |result| {
                    let _ = tx.send(result);
                },
                notify::Config::default(),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "system prompt watcher failed to start");
                    return;
                }
            };
            if let Err(error) = watcher.watch(&home, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, "system prompt watcher could not watch home");
                return;
            }

            let mut last_reload = std::time::Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now);
            while let Ok(result) = rx.recv() {
                let Ok(event) = result else {
                    continue;
                };
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) && event.paths.iter().any(|path| Self::is_system_md(path));
                if !relevant {
                    continue;
                }
                if last_reload.elapsed() < Duration::from_millis(200) {
                    continue;
                }
                last_reload = std::time::Instant::now();
                store.reload();
                let _ = events.send(ServerEvent::SystemPromptChanged);
            }
        });
    }
}

fn read_file_text(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(FILE_NAME)).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn build_prompt(text: Option<String>, browser_hub: Option<denia_tools::BrowserHub>) -> SystemPrompt {
    // server 部署有应答通道:系统提示词与工具注册必须同步带上 `ask`
    // (schema + tool:ask 纪律段),模型可见即模型可执行。
    let mut prompt = match (text, browser_hub) {
        (Some(persona), Some(hub)) => {
            denia_tools::shipped_with_persona_and_browser_and_ask(persona, Some(hub), true).0
        }
        (Some(persona), None) => {
            denia_tools::shipped_with_persona_and_browser_and_ask(persona, None, true).0
        }
        (None, Some(hub)) => denia_tools::default_shipped_with_browser_and_ask(Some(hub), true).0,
        (None, None) => denia_tools::default_shipped_with_browser_and_ask(None, true).0,
    };
    // server 部署总是注册宿主能力工具(委派/后台任务/技能),其纪律段
    // 归位系统提示词(原先是 [denia 能力上下文] 注入消息里的静态内容)。
    denia_tools::register_capability_prompt_sections(&mut prompt)
        .expect("capability prompt sections are valid");
    // 项目记忆纪律段同批注册;运行中的进退(记忆关闭不注入)由
    // agent-loop 的 assemble 阶段按 runtime.memory_root_for 过滤。
    denia_tools::register_memory_prompt_section(&mut prompt)
        .expect("memory prompt section is valid");
    prompt
}
