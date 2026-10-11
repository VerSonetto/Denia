//! 请求前准入:一次物理尝试的调用身份、账本判定与拒绝语义。
//!
//! 位置的选择:registry 的 `with_retry` 闭包内、`adapter.stream` 之前是唯一
//! "每次物理尝试都会经过"的位置。上层调用者只看得见一次逻辑调用,注册表
//! 内部的退避重试、replay 降级重发都在这一层发生 —— 准入放在上层,重试就
//! 是绕过预算的暗道(实测:一次 setup 失败最多再发 5 次,账上只记 1 次)。
//!
//! registry 既不持有会话身份也不持有账本,所以两者都由调用方经
//! [`RequestTicket`] 注入:身份由调用方构造(它才知道 session/turn/step 与
//! 来源),账本是 `&dyn RequestAdmission`。
//!
//! 计费策略不在这里:llm 只原样传递来源分类,"哪些来源吃用户预算"只有一处
//! 判据(`denia_token_meter::RequestKind::metered`)。

use denia_core::error::{LlmError, codes};

/// 一次请求的来源分类(由调用方判定,llm 层不解释)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// 普通 step 的主请求。
    UserStep,
    /// 输出截断后的续写请求。
    Continuation,
    /// 压缩摘要请求。
    Compaction,
    /// 后台任务/子代理消息触发的唤醒轮请求。
    Wake,
    /// 子代理自身的请求。
    Subagent,
    /// 会话标题生成。
    SessionTitle,
    /// `/api/llm` 连通性测试。
    Connectivity,
    /// git 摘要等宿主辅助调用。
    Git,
    /// 未分类的宿主调用(账本按计入预算兜底)。
    Other,
}

/// 一次物理请求尝试的身份。
///
/// `attempt` 由 registry 填写(同一次逻辑调用里第几次真的发出请求),
/// `sequence` 由调用方填写(同一个 step 里第几次建流)。两者一起构成去重键:
/// 显式重试与重建请求都是新身份,不会被当成重复调用挡掉或漏计;同一个物理
/// 尝试被两层同时准入时才是同一个身份(幂等)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestCall {
    pub session: String,
    pub turn: u32,
    pub step: u32,
    pub kind: RequestKind,
    /// 调用方给出的逻辑调用序号(同一 step 里第几次建流;旁路调用给 0)。
    pub sequence: u32,
    pub attempt: u32,
}

/// 账本给出的拒绝理由,原样进 `LlmError(BUDGET_EXHAUSTED)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRefusal {
    /// 边界名(步骤/请求/主动执行时间),用于对账与界面分组。
    pub boundary: String,
    pub limit: u64,
    pub used: u64,
    /// 可直接展示给用户的结论(已是完整句子)。
    pub message: String,
}

/// 请求准入账本:在请求发出之前回答"还能不能再来一次"。
///
/// 实现方必须是 `Send + Sync`(registry 在任意任务里调用它),且必须自己
/// 处理并发(共享句柄 + 内部锁)。
pub trait RequestAdmission: Send + Sync {
    fn admit(&self, call: &RequestCall) -> Result<(), AdmissionRefusal>;
}

/// 一次调用的准入票据:账本 + 调用身份。
pub struct RequestTicket<'a> {
    pub admission: &'a dyn RequestAdmission,
    pub call: RequestCall,
}

impl<'a> RequestTicket<'a> {
    /// `call.attempt` 会被 registry 覆盖(从 1 开始计物理尝试)。
    pub fn new(admission: &'a dyn RequestAdmission, call: RequestCall) -> Self {
        Self { admission, call }
    }

    /// 该物理尝试的准入判定。
    pub(crate) fn admit(&self, attempt: u32) -> Result<(), LlmError> {
        let call = RequestCall {
            attempt,
            ..self.call.clone()
        };
        self.admission.admit(&call).map_err(|refusal| {
            LlmError::new(
                codes::BUDGET_EXHAUSTED,
                format!(
                    "{}(边界:{}已用 {}/{};计量版本见账本)",
                    refusal.message, refusal.boundary, refusal.used, refusal.limit
                ),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use denia_core::stream::{ContentBlock, FinishReason, StreamChunk, TokenUsage};
    use futures::StreamExt;

    use crate::{ChunkStream, GenerateRequest, LlmAdapter, LlmRegistry, ProviderInfo, RetryPolicy};

    /// 每次调用返回一段被截断的响应(带 usage),并记录调用次数。
    struct CountingAdapter {
        calls: AtomicU32,
        /// 前几次尝试以可重试码失败(注册表内部重试路径)。
        fail_first: u32,
    }

    #[async_trait::async_trait]
    impl LlmAdapter for CountingAdapter {
        fn provider_info(&self, provider: &str) -> ProviderInfo {
            ProviderInfo {
                id: provider.to_string(),
                name: provider.to_string(),
            }
        }

        async fn list_models(&self, _provider: &str) -> Result<Vec<crate::LlmModelInfo>, LlmError> {
            Ok(Vec::new())
        }

        async fn resolve_model(
            &self,
            provider: &str,
            model: &str,
        ) -> Result<crate::LlmResolvedModelInfo, LlmError> {
            Ok(crate::LlmResolvedModelInfo {
                info: crate::LlmModelInfo {
                    provider: provider.to_string(),
                    id: model.to_string(),
                    name: model.to_string(),
                    description: None,
                    input_modalities: vec!["text".to_string()],
                },
                context_window: Some(128_000),
                default_max_tokens: Some(4_096),
                reasoning: None,
            })
        }

        async fn stream(
            &self,
            _provider: &str,
            _request: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fail_first {
                return Err(LlmError::new(codes::EMPTY_RESPONSE, "empty response"));
            }
            let chunks: Vec<Result<StreamChunk, denia_core::error::LlmFailure>> = vec![
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text { text: "ok".into() },
                }),
                Ok(StreamChunk::Usage {
                    usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 1,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    /// 记录每次准入的调用身份;可配置为拒绝。
    struct RecordingGate {
        calls: Mutex<Vec<RequestCall>>,
        refuse: Option<AdmissionRefusal>,
    }

    impl RecordingGate {
        fn new(refuse: Option<AdmissionRefusal>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                refuse,
            }
        }

        fn seen(&self) -> Vec<RequestCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RequestAdmission for RecordingGate {
        fn admit(&self, call: &RequestCall) -> Result<(), AdmissionRefusal> {
            self.calls.lock().unwrap().push(call.clone());
            match &self.refuse {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(()),
            }
        }
    }

    fn registry(retries: u32) -> (LlmRegistry, std::sync::Arc<CountingAdapter>) {
        let registry = LlmRegistry::new();
        let adapter = std::sync::Arc::new(CountingAdapter {
            calls: AtomicU32::new(0),
            fail_first: 0,
        });
        registry
            .register(
                &["mock".to_string()],
                adapter.clone(),
                RetryPolicy {
                    max_retries: retries,
                    initial_delay_ms: 0,
                    max_delay_ms: 0,
                    ..RetryPolicy::default()
                },
            )
            .unwrap();
        (registry, adapter)
    }

    /// 前 `fail_first` 次尝试以可重试码失败:用来验证注册表内部重试也逐次准入。
    fn flaky_registry(
        retries: u32,
        fail_first: u32,
    ) -> (LlmRegistry, std::sync::Arc<CountingAdapter>) {
        let registry = LlmRegistry::new();
        let adapter = std::sync::Arc::new(CountingAdapter {
            calls: AtomicU32::new(0),
            fail_first,
        });
        registry
            .register(
                &["mock".to_string()],
                adapter.clone(),
                RetryPolicy {
                    max_retries: retries,
                    initial_delay_ms: 0,
                    max_delay_ms: 0,
                    ..RetryPolicy::default()
                },
            )
            .unwrap();
        (registry, adapter)
    }

    fn request() -> GenerateRequest {
        GenerateRequest {
            model: "mock-1".to_string(),
            reasoning_effort: None,
            messages: Vec::new(),
            system: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            stop: Vec::new(),
        }
    }

    fn ticket<'a>(gate: &'a dyn RequestAdmission, kind: RequestKind) -> RequestTicket<'a> {
        RequestTicket::new(
            gate,
            RequestCall {
                session: "s1".to_string(),
                turn: 1,
                step: 1,
                kind,
                sequence: 0,
                attempt: 0,
            },
        )
    }

    #[tokio::test]
    async fn admission_runs_before_every_physical_attempt() {
        let (registry, adapter) = registry(2);
        let gate = RecordingGate::new(None);
        let stream = registry
            .stream_admitted(
                "mock",
                &request(),
                None,
                Some(&ticket(&gate, RequestKind::UserStep)),
            )
            .await
            .expect("admitted stream");
        // 准入发生在请求建立之前,而不是等流被消费。
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        let _: Vec<_> = stream.collect().await;
        let calls = gate.seen();
        assert_eq!(calls.len(), 1, "一次物理尝试一次准入");
        assert_eq!(calls[0].kind, RequestKind::UserStep);
        assert_eq!(calls[0].attempt, 1, "registry 补物理尝试序号");
        assert_eq!(calls[0].session, "s1");
    }

    #[tokio::test]
    async fn refusal_happens_before_the_request_leaves() {
        let (registry, adapter) = registry(0);
        let gate = RecordingGate::new(Some(AdmissionRefusal {
            boundary: "模型请求尝试".into(),
            limit: 256,
            used: 256,
            message: "执行预算已用尽".into(),
        }));
        let error = match registry
            .stream_admitted(
                "mock",
                &request(),
                None,
                Some(&ticket(&gate, RequestKind::UserStep)),
            )
            .await
        {
            Ok(_) => panic!("预算不足必须拒绝"),
            Err(error) => error,
        };
        assert_eq!(error.code, codes::BUDGET_EXHAUSTED);
        assert!(error.message.contains("执行预算已用尽"));
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            0,
            "被拒绝的请求不得发给提供方"
        );
    }

    #[tokio::test]
    async fn bypass_sources_are_passed_through_verbatim() {
        // 旁路(标题生成)走同一个准入入口,但来源分类原样传到账本:是否
        // 计费由账本判据决定,llm 层不改写来源。
        let (registry, adapter) = registry(0);
        let gate = RecordingGate::new(None);
        let stream = registry
            .stream_admitted(
                "mock",
                &request(),
                None,
                Some(&ticket(&gate, RequestKind::SessionTitle)),
            )
            .await
            .expect("bypass stream");
        let _: Vec<_> = stream.collect().await;
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(gate.seen()[0].kind, RequestKind::SessionTitle);
    }

    #[tokio::test]
    async fn registry_retries_are_admitted_one_by_one() {
        // 注册表内部重试在上层看是**一次**调用:准入若放在上层,这里就少记
        // 两次真金白银的尝试。
        let (registry, adapter) = flaky_registry(3, 2);
        let gate = RecordingGate::new(None);
        let stream = registry
            .stream_admitted(
                "mock",
                &request(),
                None,
                Some(&ticket(&gate, RequestKind::UserStep)),
            )
            .await
            .expect("third attempt succeeds");
        let _: Vec<_> = stream.collect().await;
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 3);
        let attempts: Vec<u32> = gate.seen().iter().map(|call| call.attempt).collect();
        assert_eq!(attempts, vec![1, 2, 3], "每次物理尝试都过准入");
    }

    #[tokio::test]
    async fn a_refusal_mid_retry_stops_the_next_attempt() {
        // 账本在第 2 次尝试前拒绝:第 3 次请求不会发出。
        let (registry, adapter) = flaky_registry(3, 3);
        struct RefuseSecond {
            calls: Mutex<Vec<u32>>,
        }
        impl RequestAdmission for RefuseSecond {
            fn admit(&self, call: &RequestCall) -> Result<(), AdmissionRefusal> {
                self.calls.lock().unwrap().push(call.attempt);
                if call.attempt >= 2 {
                    return Err(AdmissionRefusal {
                        boundary: "模型请求尝试".into(),
                        limit: 1,
                        used: 1,
                        message: "执行预算已用尽:本轮模型请求尝试已达上限 1 次".into(),
                    });
                }
                Ok(())
            }
        }
        let gate = RefuseSecond {
            calls: Mutex::new(Vec::new()),
        };
        let error = match registry
            .stream_admitted(
                "mock",
                &request(),
                None,
                Some(&ticket(&gate, RequestKind::UserStep)),
            )
            .await
        {
            Ok(_) => panic!("second admission must refuse"),
            Err(error) => error,
        };
        assert_eq!(error.code, codes::BUDGET_EXHAUSTED);
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            1,
            "拒绝之后不得再发请求"
        );
        assert_eq!(gate.calls.lock().unwrap().clone(), vec![1, 2]);
    }

    #[tokio::test]
    async fn the_caller_sequence_is_passed_through_untouched() {
        // 同一 step 里第二次建流:registry 只改 attempt,不动调用方的 sequence。
        let (registry, adapter) = registry(0);
        let gate = RecordingGate::new(None);
        let ticket = RequestTicket::new(
            &gate,
            RequestCall {
                session: "s1".to_string(),
                turn: 1,
                step: 1,
                kind: RequestKind::UserStep,
                sequence: 2,
                attempt: 0,
            },
        );
        let stream = registry
            .stream_admitted("mock", &request(), None, Some(&ticket))
            .await
            .expect("admitted stream");
        let _: Vec<_> = stream.collect().await;
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        let calls = gate.seen();
        assert_eq!(calls[0].sequence, 2);
        assert_eq!(calls[0].attempt, 1);
    }

    #[tokio::test]
    async fn ungated_entry_point_never_consults_a_ledger() {
        // 标题/连通测试/git 走未接准入的入口:没有票据就没有账本可撞。
        let (registry, adapter) = registry(0);
        let stream = registry
            .stream("mock", &request(), None)
            .await
            .expect("ungated stream");
        let _: Vec<_> = stream.collect().await;
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    }
}
