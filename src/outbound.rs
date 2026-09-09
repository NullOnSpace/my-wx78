//! 出向管线：纯 D 连接器（push.v1 契约）。
//!
//! 消费 `stream:rendered:qq`（streams-lib consumer group `my-wx78`；render-hub C 层
//! 产出 proto 编码的 [`RenderEnvelope`]），按 payload 链序尝试（主 payload + 预渲染
//! fallback，成功即停），每个信封的发送结果 XADD 一条 [`SendReceipt`] 到
//! `stream:receipts`。
//!
//! 本连接器不理解消息语义（不变量 #4）：只把 envelope 身份字段原样拷贝进
//! receipt，除声明的 [`PayloadKind`] 外不解释 payload 内容。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use canonical_msg::prost::Message;
use canonical_msg::{Channel, PayloadKind, RenderEnvelope, SendReceipt};
use qq_bot::{DirectMessageClient, DirectMessageError};
use redis::aio::MultiplexedConnection;
use streams_lib::{ConsumerConfig, DATA_FIELD, consume_loop, xadd_message};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use dispatch_core::{
    DispatchResult, RetryError, RetryPolicy, TokenBucket, build_receipt, with_retry_if,
};

/// Redis 订阅异常后的重连退避上界。
const MAX_REDIS_BACKOFF_SECS: u64 = 30;

/// 出向 consumer group 名（组名=服务名，streams-lib 约定）。
const OUTBOUND_GROUP: &str = "my-wx78";

/// 发送原语抽象（仿 feishu-bot `CardSender`）：payload 链逻辑用手写 mock
/// 测试，真实实现包 `DirectMessageClient`。返回平台的 message id
/// （receipt 的 `platform_message_id`）。
trait QqSender: Send + Sync {
    async fn send_markdown(&self, target: &str, content: &str) -> Result<String, SendError>;
    async fn send_text(&self, target: &str, content: &str) -> Result<String, SendError>;
}

/// 单次平台发送的错误，可重试性在此分类（dispatch-core 不含错误语义）。
#[derive(Debug)]
enum SendError {
    /// 网络层错误（连接失败/超时）：值得重试。
    Transient(String),
    /// 平台拒绝（非 2xx 响应）或载荷问题（UTF-8/未知 kind）：重试无意义。
    Permanent(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Transient(message) => write!(f, "transient send error: {message}"),
            SendError::Permanent(message) => write!(f, "permanent send error: {message}"),
        }
    }
}

/// SDK 错误 → 重试分类。
///
/// SDK 的 `Http` 变体只覆盖真正的网络层失败（连接/超时，reqwest 未对 HTTP
/// 状态码报错）；非 2xx 响应走 `Api(String)`，该变体不携带 HTTP 状态码——
/// 无法区分 5xx 与 400/权限类，保守全部按"平台拒绝"处理（不重试）。
/// QQ 场景下非 2xx 主要是 400 配额/审核拒绝，重试反而会烧配额。
fn map_send_error(error: DirectMessageError) -> SendError {
    match error {
        DirectMessageError::Http(_) => SendError::Transient(error.to_string()),
        _ => SendError::Permanent(error.to_string()),
    }
}

impl QqSender for DirectMessageClient {
    async fn send_markdown(&self, target: &str, content: &str) -> Result<String, SendError> {
        DirectMessageClient::send_markdown(self, target, content)
            .await
            .map(|response| response.id)
            .map_err(map_send_error)
    }

    async fn send_text(&self, target: &str, content: &str) -> Result<String, SendError> {
        DirectMessageClient::send_text(self, target, content.to_owned())
            .await
            .map(|response| response.id)
            .map_err(map_send_error)
    }
}

/// 单个信封 payload 链的结局：是否送达、平台总尝试次数、最后错误、平台回执 id。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchOutcome {
    ok: bool,
    attempts: u32,
    error: Option<String>,
    platform_message_id: Option<String>,
}

impl DispatchResult for DispatchOutcome {
    fn ok(&self) -> bool {
        self.ok
    }

    fn attempts(&self) -> u32 {
        self.attempts
    }

    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// 拒绝本连接器不得处理的信封。
///
/// `Err(reason)` 表示：WARN + 丢弃、无 receipt——信封违反了 rendered 频道
/// 契约，为它回执会误导 B。
fn validate_envelope(envelope: &RenderEnvelope) -> Result<(), &'static str> {
    match Channel::try_from(envelope.channel) {
        Ok(Channel::Qq) => {}
        Ok(Channel::Feishu) => return Err("envelope is for the Feishu channel"),
        Ok(_) => return Err("envelope channel is UNSPECIFIED"),
        Err(_) => return Err("envelope carries an unknown Channel value"),
    }
    if envelope.target_id.trim().is_empty() {
        return Err("envelope target_id is empty");
    }
    Ok(())
}

/// 发送 payload 链中的一个。解码错误（非 UTF-8）与不支持的 kind 记为该
/// payload 的失败；调用方移到下一个 fallback。
async fn try_send<S: QqSender>(
    sender: &S,
    target: &str,
    kind: i32,
    data: &[u8],
) -> Result<String, SendError> {
    let kind = PayloadKind::try_from(kind).map_err(|_| {
        SendError::Permanent(format!("payload kind {kind} is not a known PayloadKind"))
    })?;
    match kind {
        PayloadKind::QqMarkdown => {
            // msg_type=2 原生 markdown，实测私聊可用（qq-bot docs/API_VERIFICATION.md §八）。
            let content = std::str::from_utf8(data).map_err(|e| {
                SendError::Permanent(format!("markdown payload is not valid UTF-8: {e}"))
            })?;
            sender.send_markdown(target, content).await
        }
        PayloadKind::Text => {
            let content = std::str::from_utf8(data).map_err(|e| {
                SendError::Permanent(format!("text payload is not valid UTF-8: {e}"))
            })?;
            sender.send_text(target, content).await
        }
        PayloadKind::Json | PayloadKind::Html | PayloadKind::Unspecified => {
            Err(SendError::Permanent(format!(
                "payload kind {kind:?} is not supported by the QQ connector"
            )))
        }
    }
}

/// 按序尝试 `[主 payload] + fallback`，成功即停。
///
/// 每个 payload 的平台发送先取限流令牌，再走 dispatch-core `with_retry_if`
/// （Transient 重试 / Permanent 立即终局）。`attempts` 是跨整条链的平台
/// 总尝试次数（receipts.attempts）。
///
/// 返回 [`None`] 表示限流/退避等待期间收到取消信号——服务正在关机，
/// 本次投递被外部放弃，不再发 receipt。
async fn dispatch_envelope<S: QqSender>(
    sender: &S,
    envelope: &RenderEnvelope,
    bucket: &TokenBucket,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
) -> Option<DispatchOutcome> {
    let target = &envelope.target_id;
    let chain = std::iter::once((envelope.payload_kind, envelope.payload.as_slice())).chain(
        envelope
            .fallback
            .iter()
            .map(|payload| (payload.kind, payload.data.as_slice())),
    );
    let mut attempts: u32 = 0;
    let mut last_error: Option<String> = None;

    for (kind, data) in chain {
        match bucket.acquire(cancel).await {
            dispatch_core::Acquisition::Cancelled => return None,
            dispatch_core::Acquisition::Granted => {}
            dispatch_core::Acquisition::Throttled { wait_ms } => {
                tracing::debug!(task_id = %envelope.task_id, wait_ms, "rate limited, token acquired after wait");
            }
        }

        match with_retry_if(
            policy,
            cancel,
            |error: &SendError| matches!(error, SendError::Transient(_)),
            || try_send(sender, target, kind, data),
        )
        .await
        {
            Ok(message_id) => {
                return Some(DispatchOutcome {
                    ok: true,
                    attempts: attempts + 1,
                    error: None,
                    platform_message_id: Some(message_id),
                });
            }
            Err(RetryError::Exhausted {
                attempts: payload_attempts,
                last_error: error,
            }) => {
                attempts += payload_attempts;
                tracing::warn!(
                    task_id = %envelope.task_id,
                    payload_kind = kind,
                    %error,
                    "payload attempt failed"
                );
                last_error = Some(error.to_string());
            }
            Err(RetryError::Cancelled) => return None,
        }
    }

    Some(DispatchOutcome {
        ok: false,
        attempts,
        error: last_error,
        platform_message_id: None,
    })
}

/// XADD 一条 receipt；失败只损失一次可观测性，绝不能拖垮/中断消息循环。
async fn publish_receipt(
    conn: &mut MultiplexedConnection,
    receipts_stream: &str,
    receipt: &SendReceipt,
) {
    let bytes = receipt.encode_to_vec();
    if let Err(error) = xadd_message(conn, receipts_stream, &bytes).await {
        tracing::warn!(
            task_id = %receipt.task_id,
            %error,
            "publishing receipt failed (observability lost for this send)"
        );
    }
}

/// 消费 rendered stream 直至取消：decode → validate → 限流/重试发送 → receipt。
///
/// 传输走 streams-lib [`consume_loop`]（group=`my-wx78`）：handler Ok = XACK；
/// handler Err = 留在 pending 待 XAUTOCLAIM 接管重试，超限进 DLQ。
///
/// `sender`/`bucket`/`policy` 以 `Arc` 传入：consume_loop 要求 handler 返回的
/// future 为 `'static`（类型参数默认带 `'static` 界），闭包只能捕获 owned 数据。
///
/// # Errors
/// Redis 连接错误时返回 Err（由外层 [`run_outbound`] 退避重连）；
/// 入向管线独立于本任务，不受影响。
struct LoopChannels<'a> {
    redis_url: &'a str,
    rendered_stream: &'a str,
    receipts_stream: &'a str,
    /// 入向回复兜底 openid（仅用于 target 不一致的 WARN 观测）。
    open_id: &'a str,
}

async fn run_outbound_loop<S: QqSender + 'static>(
    channels: &LoopChannels<'_>,
    sender: Arc<S>,
    bucket: Arc<TokenBucket>,
    policy: Arc<RetryPolicy>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let LoopChannels {
        redis_url,
        rendered_stream,
        receipts_stream,
        open_id,
    } = *channels;
    let client = redis::Client::open(redis_url)?;
    let publish_conn = client
        .get_multiplexed_tokio_connection()
        .await
        .context("connecting redis for receipts publishing")?;
    let cfg = ConsumerConfig::new(rendered_stream, OUTBOUND_GROUP);
    let loop_cancel = cancel.clone();

    consume_loop(client, cfg, cancel, move |entry| {
        let mut publish_conn = publish_conn.clone();
        let sender = Arc::clone(&sender);
        let bucket = Arc::clone(&bucket);
        let policy = Arc::clone(&policy);
        let loop_cancel = loop_cancel.clone();
        let rendered_stream = rendered_stream.to_owned();
        let receipts_stream = receipts_stream.to_owned();
        let open_id = open_id.to_owned();
        async move {
            let raw: Vec<u8> = entry
                .get(DATA_FIELD)
                .ok_or_else(|| anyhow::anyhow!("entry {} missing data field", entry.id))?;
            let envelope = match RenderEnvelope::decode(raw.as_slice()) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // 毒丸（C 层 bug/契约漂移）：留 pending 重试，超限进 DLQ。
                    return Err(anyhow::anyhow!(
                        "undecodable envelope on {rendered_stream}: {error}"
                    ));
                }
            };
            if let Err(reason) = validate_envelope(&envelope) {
                tracing::warn!(
                    task_id = %envelope.task_id,
                    reason,
                    "rejecting envelope"
                );
                // 契约违规 = 消费掉（XACK）但无 receipt：为它回执会误导 B。
                return Ok(());
            }

            // target 语义：`RenderEnvelope.target_id` 是投递目标的唯一决策者
            //（B 决策，不变量 #7），原样使用。env 的 `QQ_OPEN_ID` 不是出向目标：
            // 它只是入向回复的兜底 openid（入向事件缺 user_openid 时用）。真实
            // openid 只能从入向消息拿到，部署期出向收到的静态 openid 与 env 值
            // 相同纯属巧合；两者不一致时 WARN 观测、仍按 envelope.target_id 发。
            if envelope.target_id != open_id {
                tracing::warn!(
                    task_id = %envelope.task_id,
                    envelope_target = %envelope.target_id,
                    env_open_id = %open_id,
                    "envelope target differs from QQ_OPEN_ID; sending to envelope.target_id (env value is inbound-reply fallback only)"
                );
            }

            let outcome = match dispatch_envelope(
                sender.as_ref(),
                &envelope,
                &bucket,
                &policy,
                &loop_cancel,
            )
            .await
            {
                Some(outcome) => outcome,
                None => {
                    // 关机途中被外部放弃：Err → 不 XACK，留 pending 由下个实例补投。
                    return Err(anyhow::anyhow!("dispatch cancelled during shutdown"));
                }
            };
            if outcome.ok {
                tracing::info!(
                    task_id = %envelope.task_id,
                    message_id = %envelope.message_id,
                    target = %envelope.target_id,
                    attempts = outcome.attempts,
                    "sent"
                );
            } else {
                tracing::error!(
                    task_id = %envelope.task_id,
                    message_id = %envelope.message_id,
                    target = %envelope.target_id,
                    attempts = outcome.attempts,
                    error = outcome.error.as_deref().unwrap_or("?"),
                    "all payloads failed"
                );
            }
            publish_receipt(
                &mut publish_conn,
                &receipts_stream,
                &build_receipt(&envelope, &outcome, outcome.platform_message_id.as_deref()),
            )
            .await;
            Ok(())
        }
    })
    .await
}

/// 带重连的外出向循环：Redis 故障按指数退避重连，取消信号可打断等待。
pub async fn run_outbound(
    config: AppConfig,
    sender: Arc<DirectMessageClient>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // 突发容量与回填速率同值（30/min）：QQ 主动消息有月度配额与审核，
    // 瞬时突发不增加总配额消耗，只减少排队等待。
    let bucket = Arc::new(TokenBucket::new(
        config.outbound.rate_limit_per_min,
        f64::from(config.outbound.rate_limit_per_min),
    ));
    let policy = Arc::new(RetryPolicy {
        max_attempts: config.outbound.retry_max_attempts,
        ..RetryPolicy::default()
    });

    let mut backoff_secs: u64 = 1;
    loop {
        let channels = LoopChannels {
            redis_url: &config.redis.url,
            rendered_stream: &config.redis.rendered_stream,
            receipts_stream: &config.redis.receipts_stream,
            open_id: &config.qq_bot.open_id,
        };
        match run_outbound_loop(
            &channels,
            Arc::clone(&sender),
            Arc::clone(&bucket),
            Arc::clone(&policy),
            cancel.clone(),
        )
        .await
        {
            Ok(()) => {
                tracing::info!("outbound loop stopped (shutdown)");
                return Ok(());
            }
            Err(error) => {
                tracing::error!(%error, backoff_secs, "outbound loop failed, reconnecting");
            }
        }
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("收到关机信号，停止出向重连");
                return Ok(());
            }
            () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {
                backoff_secs = (backoff_secs * 2).min(MAX_REDIS_BACKOFF_SECS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use canonical_msg::{Payload, PayloadKind};
    use qq_bot::DirectMessageError;

    use super::*;

    /// 确定性 mock：markdown 恒失败，文本恒成功。
    struct MarkdownFailsTextOkSender;

    impl QqSender for MarkdownFailsTextOkSender {
        async fn send_markdown(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Err(SendError::Permanent("markdown send failed".into()))
        }

        async fn send_text(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Ok("text-msg-id-1".into())
        }
    }

    /// 确定性 mock：markdown 成功。
    struct MarkdownOkSender;

    impl QqSender for MarkdownOkSender {
        async fn send_markdown(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Ok("md-msg-id-1".into())
        }

        async fn send_text(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Err(SendError::Permanent("text send failed".into()))
        }
    }

    /// 确定性 mock：全部失败。
    struct AllFailSender;

    impl QqSender for AllFailSender {
        async fn send_markdown(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Err(SendError::Permanent("markdown send failed".into()))
        }

        async fn send_text(&self, _target: &str, _content: &str) -> Result<String, SendError> {
            Err(SendError::Permanent("text send failed".into()))
        }
    }

    fn test_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
            jitter: dispatch_core::Jitter::None,
        }
    }

    fn test_bucket() -> TokenBucket {
        TokenBucket::new(1000, 60_000.0)
    }

    fn envelope_with(
        channel: Channel,
        target_id: &str,
        payload_kind: PayloadKind,
        payload: &[u8],
        fallback: Vec<Payload>,
    ) -> RenderEnvelope {
        RenderEnvelope {
            task_id: "task-1".into(),
            message_id: "msg-1".into(),
            channel: channel as i32,
            target_id: target_id.into(),
            payload_kind: payload_kind as i32,
            payload: payload.to_vec(),
            fallback,
        }
    }

    async fn dispatch<S: QqSender>(
        sender: &S,
        envelope: &RenderEnvelope,
    ) -> Option<DispatchOutcome> {
        dispatch_envelope(
            sender,
            envelope,
            &test_bucket(),
            &test_policy(),
            &CancellationToken::new(),
        )
        .await
    }

    #[tokio::test]
    async fn dispatch_main_success_stops_at_first_payload() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback".to_vec(),
            }],
        );
        let outcome = dispatch(&MarkdownOkSender, &envelope).await.unwrap();
        assert_eq!(
            outcome,
            DispatchOutcome {
                ok: true,
                attempts: 1,
                error: None,
                platform_message_id: Some("md-msg-id-1".into()),
            }
        );
    }

    #[tokio::test]
    async fn dispatch_falls_back_when_markdown_fails() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback".to_vec(),
            }],
        );
        let outcome = dispatch(&MarkdownFailsTextOkSender, &envelope)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            DispatchOutcome {
                ok: true,
                attempts: 2,
                error: None,
                platform_message_id: Some("text-msg-id-1".into()),
            }
        );
    }

    #[tokio::test]
    async fn dispatch_retries_transient_then_falls_back() {
        // markdown 永久失败（不重试）→ text 第一次成功：链内共 2 次平台调用。
        // 换 AllFailSender 验证可重试错误会耗尽 max_attempts：
        struct TransientTwiceThenPermanent;

        impl QqSender for TransientTwiceThenPermanent {
            async fn send_markdown(
                &self,
                _target: &str,
                _content: &str,
            ) -> Result<String, SendError> {
                Err(SendError::Transient("timeout".into()))
            }

            async fn send_text(&self, _target: &str, _content: &str) -> Result<String, SendError> {
                Ok("text-msg-id-2".into())
            }
        }

        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback".to_vec(),
            }],
        );
        // max_attempts=2：markdown 烧 2 次 transient 重试，text 第 3 次成功。
        let outcome = dispatch(&TransientTwiceThenPermanent, &envelope)
            .await
            .unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.attempts, 3);
        assert_eq!(
            outcome.platform_message_id.as_deref(),
            Some("text-msg-id-2")
        );
    }

    #[tokio::test]
    async fn dispatch_all_failed_records_last_error_and_attempts() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback".to_vec(),
            }],
        );
        let outcome = dispatch(&AllFailSender, &envelope).await.unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.attempts, 2);
        assert_eq!(
            outcome.error.as_deref(),
            Some("permanent send error: text send failed")
        );
        assert_eq!(outcome.platform_message_id, None);
    }

    #[tokio::test]
    async fn dispatch_unsupported_main_kind_tries_fallback() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::Json,
            br#"{"schema":"2.0"}"#,
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback".to_vec(),
            }],
        );
        let outcome = dispatch(&MarkdownFailsTextOkSender, &envelope)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            DispatchOutcome {
                ok: true,
                attempts: 2,
                error: None,
                platform_message_id: Some("text-msg-id-1".into()),
            }
        );
    }

    #[test]
    fn validate_rejects_non_qq_channel() {
        let envelope = envelope_with(Channel::Feishu, "ou_x", PayloadKind::Text, b"x", vec![]);
        assert!(validate_envelope(&envelope).is_err());
    }

    #[test]
    fn validate_rejects_unspecified_channel() {
        let envelope = envelope_with(
            Channel::Unspecified,
            "ou_x",
            PayloadKind::Text,
            b"x",
            vec![],
        );
        assert!(validate_envelope(&envelope).is_err());
    }

    #[test]
    fn validate_rejects_unknown_channel_value() {
        let mut envelope = envelope_with(Channel::Qq, "ou_x", PayloadKind::Text, b"x", vec![]);
        envelope.channel = 99;
        assert!(validate_envelope(&envelope).is_err());
    }

    #[test]
    fn validate_rejects_empty_target() {
        let envelope = envelope_with(Channel::Qq, "  ", PayloadKind::Text, b"x", vec![]);
        assert!(validate_envelope(&envelope).is_err());
    }

    #[test]
    fn validate_accepts_wellformed_envelope() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![],
        );
        assert!(validate_envelope(&envelope).is_ok());
    }

    #[test]
    fn send_error_classification_api_rejection_not_retryable() {
        // Http 变体无公共构造器（reqwest），网络错 → Transient 的分支由
        // `dispatch_retries_transient_then_falls_back` 用 mock Transient 覆盖；
        // 这里验证平台拒绝/响应解析错误一律 Permanent（不重试）。
        let rejection = DirectMessageError::Api("400 quota exceeded".into());
        assert!(matches!(map_send_error(rejection), SendError::Permanent(_)));

        let parse = DirectMessageError::Json(
            serde_json::from_str::<serde_json::Value>("{bad").unwrap_err(),
        );
        assert!(matches!(map_send_error(parse), SendError::Permanent(_)));
    }

    #[test]
    fn receipt_echoes_envelope_and_outcome() {
        let envelope = envelope_with(
            Channel::Qq,
            "openid_x",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![],
        );
        let outcome = DispatchOutcome {
            ok: true,
            attempts: 1,
            error: None,
            platform_message_id: Some("md-msg-id-1".into()),
        };
        let receipt = build_receipt(&envelope, &outcome, outcome.platform_message_id.as_deref());
        assert_eq!(receipt.task_id, "task-1");
        assert_eq!(receipt.channel, Channel::Qq as i32);
        assert_eq!(receipt.target_id, "openid_x");
        assert!(receipt.ok);
        assert_eq!(receipt.error, "");
        assert_eq!(receipt.attempts, 1);
        assert_eq!(receipt.platform_message_id, "md-msg-id-1");
        assert!(receipt.sent_at.is_some());
    }

    #[test]
    fn receipt_failure_carries_error_and_attempts() {
        let envelope = envelope_with(Channel::Qq, "openid_x", PayloadKind::Text, b"x", vec![]);
        let outcome = DispatchOutcome {
            ok: false,
            attempts: 4,
            error: Some("permanent send error: text send failed".into()),
            platform_message_id: None,
        };
        let receipt = build_receipt(&envelope, &outcome, None);
        assert!(!receipt.ok);
        assert_eq!(receipt.error, "permanent send error: text send failed");
        assert_eq!(receipt.attempts, 4);
        assert_eq!(receipt.platform_message_id, "");
    }

    /// 端到端（真实 Redis）：向 rendered stream XADD 合法信封，receipts stream 必须
    /// 出现身份字段逐字回显的 receipt。mock 发送器证明"循环 + receipt 接线"
    /// 正确；真实 QQ 发送不进自动化测试（外部 SaaS，项目惯例）。
    /// env 门控：`MY_WX78_TEST_REDIS=1`。
    #[tokio::test]
    async fn loop_publishes_receipt_for_valid_envelope_when_env_configured() {
        if std::env::var("MY_WX78_TEST_REDIS").as_deref() != Ok("1") {
            eprintln!("skipping: MY_WX78_TEST_REDIS != 1");
            return;
        }
        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let rendered_stream = "my-wx78-test-rendered";
        let receipts_stream = "my-wx78-test-receipts";
        let dlq_stream = format!("{rendered_stream}:dlq");
        let client = redis::Client::open(redis_url.as_str()).unwrap();
        let mut admin = client.get_multiplexed_tokio_connection().await.unwrap();
        // 清理测试残留（DEL 连 consumer group 一并删除，测试自建全新组）。
        for key in [rendered_stream, receipts_stream, dlq_stream.as_str()] {
            redis::cmd("DEL").arg(key).query_async::<i64>(&mut admin).await.unwrap();
        }

        let cancel = CancellationToken::new();
        let loop_task = tokio::spawn({
            let redis_url = redis_url.clone();
            let cancel = cancel.clone();
            async move {
                let channels = LoopChannels {
                    redis_url: &redis_url,
                    rendered_stream,
                    receipts_stream,
                    open_id: "openid_env",
                };
                run_outbound_loop(
                    &channels,
                    Arc::new(MarkdownFailsTextOkSender),
                    Arc::new(test_bucket()),
                    Arc::new(test_policy()),
                    cancel,
                )
                .await
            }
        });

        // 等 consumer group 建好（XGROUP CREATE $ 之后 XADD 的消息才会被读到，
        // 与 pub/sub 时代"等订阅完成"同理）。
        wait_for_group(&mut admin, rendered_stream, OUTBOUND_GROUP).await;

        let envelope = envelope_with(
            Channel::Qq,
            "openid_test",
            PayloadKind::QqMarkdown,
            b"# hello",
            vec![Payload {
                kind: PayloadKind::Text as i32,
                data: b"fallback text".to_vec(),
            }],
        );
        xadd_message(&mut admin, rendered_stream, &envelope.encode_to_vec())
            .await
            .unwrap();

        let receipt =
            wait_for_receipt(&mut admin, receipts_stream, Duration::from_secs(10)).await;
        assert_eq!(receipt.task_id, "task-1");
        assert_eq!(receipt.channel, Channel::Qq as i32);
        assert_eq!(receipt.target_id, "openid_test");
        // mock 拒绝 markdown；预渲染的 text fallback 顶上。
        assert!(receipt.ok);
        assert_eq!(receipt.attempts, 2);
        assert_eq!(receipt.platform_message_id, "text-msg-id-1");
        assert!(receipt.sent_at.is_some());

        cancel.cancel();
        loop_task.await.unwrap().unwrap();
    }

    /// 轮询 `XINFO GROUPS` 直到 group 出现（consume_loop 建组完成信号）。
    async fn wait_for_group(conn: &mut MultiplexedConnection, stream: &str, group: &str) {
        use std::time::Instant;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // XINFO GROUPS 每组是 name/consumers/pending/... 的扁平数组，值为
            // 字符串与整数混合：按 Value 解析后只找 name 字段。
            let groups: Vec<Vec<redis::Value>> = redis::cmd("XINFO")
                .arg("GROUPS")
                .arg(stream)
                .query_async(conn)
                .await
                .unwrap_or_default();
            let found = groups.iter().any(|fields| {
                fields.chunks(2).any(|pair| {
                    matches!(
                        (pair.first(), pair.get(1)),
                        (
                            Some(redis::Value::BulkString(key)),
                            Some(redis::Value::BulkString(value))
                        ) if key.as_slice() == b"name" && value.as_slice() == group.as_bytes()
                    )
                })
            });
            if found {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "consumer group {group} not created in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 轮询 XRANGE receipts stream 直到出现 task_id 匹配的 receipt。
    async fn wait_for_receipt(
        conn: &mut MultiplexedConnection,
        stream: &str,
        timeout: Duration,
    ) -> SendReceipt {
        use redis::AsyncCommands;
        use std::time::Instant;
        let deadline = Instant::now() + timeout;
        loop {
            let reply: redis::streams::StreamRangeReply = conn.xrange_all(stream).await.unwrap();
            for entry in &reply.ids {
                if let Some(redis::Value::BulkString(bytes)) = entry.map.get("data")
                    && let Ok(receipt) = SendReceipt::decode(bytes.as_slice())
                    && receipt.task_id == "task-1"
                {
                    return receipt;
                }
            }
            assert!(
                Instant::now() < deadline,
                "receipt not published in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
