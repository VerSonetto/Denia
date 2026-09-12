//! 浏览器侧观测缓冲:网络抓包(环形,按 tab 分桶)。
//!
//! 这些缓冲只服务 `networkList` 命令,与浏览器驱动解耦——Playwright 的
//! 事件回调把数据投进来,读取侧按 tab 查询。

use std::collections::HashMap;

use serde_json::Value;

/// 每 tab 保留的最大网络条目(环形,超出丢最旧)。
const NETWORK_LOG_CAP: usize = 200;

/// 网络抓包缓冲:tabId -> (插入序, requestId -> 条目)。
#[derive(Default)]
pub struct NetworkLog {
    /// tabId -> 有序 requestId(便于按时间列出与截断)。
    order: HashMap<String, Vec<(String, u64)>>,
    /// tabId -> requestId -> 条目。
    entries: HashMap<String, HashMap<String, Value>>,
    /// 全局单调序号(排序用)。
    counter: u64,
}

impl NetworkLog {
    /// 记录/合并一条请求(同一 requestId 的多次事件按字段合并)。
    pub fn record(&mut self, tab_id: &str, request_id: &str, entry: Value) {
        let order = self.order.entry(tab_id.to_string()).or_default();
        let is_new = !order.iter().any(|(id, _)| id == request_id);
        if is_new {
            self.counter += 1;
            order.push((request_id.to_string(), self.counter));
        }
        // merge:同一请求的多个事件(requestWillBeSent → responseReceived →
        // loadingFailed)按字段合并,新值非 null 才覆盖,保留既有字段。
        let map = self.entries.entry(tab_id.to_string()).or_default();
        let slot = map
            .entry(request_id.to_string())
            .or_insert_with(|| entry.clone());
        if let (Some(existing), Some(incoming)) = (slot.as_object_mut(), entry.as_object()) {
            for (key, value) in incoming {
                if !value.is_null() {
                    existing.insert(key.clone(), value.clone());
                }
            }
        }
        // 环形截断。
        if order.len() > NETWORK_LOG_CAP {
            let excess = order.len() - NETWORK_LOG_CAP;
            let evicted: Vec<String> = order.drain(..excess).map(|(id, _)| id).collect();
            if let Some(map) = self.entries.get_mut(tab_id) {
                for id in evicted {
                    map.remove(&id);
                }
            }
        }
    }

    /// 列出某 tab 的请求(按时间,取最近 max 条)。
    pub fn list(&self, tab_id: &str, url_filter: Option<&str>, max: u32) -> Vec<Value> {
        let Some(order) = self.order.get(tab_id) else {
            return Vec::new();
        };
        let map = self.entries.get(tab_id);
        let mut items: Vec<(u64, Value)> = order
            .iter()
            .filter_map(|(id, seq)| {
                let entry = map.and_then(|m| m.get(id))?;
                if let Some(filter) = url_filter {
                    let url = entry.get("url").and_then(Value::as_str).unwrap_or("");
                    if !url.contains(filter) {
                        return None;
                    }
                }
                Some((*seq, entry.clone()))
            })
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        let start = items.len().saturating_sub(max as usize);
        items
            .into_iter()
            .skip(start)
            .map(|(_, entry)| entry)
            .collect()
    }

    /// 取单条请求。
    pub fn entry(&self, tab_id: &str, request_id: &str) -> Option<Value> {
        self.entries
            .get(tab_id)
            .and_then(|m| m.get(request_id))
            .cloned()
    }

    /// 清空某 tab。
    pub fn clear(&mut self, tab_id: &str) {
        self.order.remove(tab_id);
        self.entries.remove(tab_id);
    }
}
