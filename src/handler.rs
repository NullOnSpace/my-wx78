use crate::redis::RedisManager;
use canonical_msg::canonical_message;
use canonical_msg::pbjson_types;
use canonical_msg::{CanonicalMessage, ChatMessage, MsgKind, Source, SourceKind};
use qq_bot::{DirectMessageClient, MessageEvent, WebSocketClient, WebSocketHandler};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{error, info};

/// 入向信源标识（`source.id`，稳定机器标识）。
const SOURCE_ID: &str = "qq-c2c";
/// 渲染用可读名。
const SOURCE_DISPLAY_NAME: &str = "QQ 私聊";
/// `dedup_key` 前缀：`qq-c2c:{platform_message_id}`（幂等知识封在 A=入向侧）。
const DEDUP_PREFIX: &str = "qq-c2c:";

pub struct RetweetHandler {
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
}

impl RetweetHandler {
    pub fn new(redis: Arc<RedisManager>, dm_client: Arc<DirectMessageClient>) -> Self {
        Self { redis, dm_client }
    }

    /// 构造入向 canonical CHAT（A 层唯一写方职责，不变量 #7）。
    ///
    /// 域约定（canonical-msg 契约注释）：
    /// - `body_text` = `body_markdown` = content 原文（纯文本是合法 markdown 子集）；
    /// - 顶层 `published_at` = `domain.chat.received_at` 同值（内容发布时间 = 消息到达入向时刻）；
    /// - `chat_id` 留空（QQ C2C 无群概念，会话身份在 `author_openid`）。
    pub fn build_canonical_chat(message: &MessageEvent) -> CanonicalMessage {
        let received_at =
            parse_platform_timestamp(&message.timestamp).unwrap_or_else(now_timestamp);
        let content = message.content.clone();

        CanonicalMessage {
            id: ulid::Ulid::generate().to_string(),
            source: Some(Source {
                kind: SourceKind::Im as i32,
                id: SOURCE_ID.to_owned(),
                display_name: SOURCE_DISPLAY_NAME.to_owned(),
            }),
            dedup_key: format!("{DEDUP_PREFIX}{}", message.id),
            kind: MsgKind::Chat as i32,
            title: String::new(),
            body_markdown: content.clone(),
            body_text: content.clone(),
            links: vec![],
            tags: vec![],
            published_at: Some(received_at),
            domain: Some(canonical_message::Domain::Chat(ChatMessage {
                platform_message_id: message.id.clone(),
                author_id: message.author.id.clone(),
                author_openid: message.author.user_openid.clone().unwrap_or_default(),
                content,
                received_at: Some(received_at),
                chat_id: String::new(),
            })),
        }
    }
}

impl WebSocketHandler for RetweetHandler {
    fn on_message(
        &self,
        message: MessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            info!(
                message_id = %message.id,
                author_id = %message.author.id,
                content = %message.content,
                "收到QQ消息，准备构造 canonical CHAT"
            );

            let canonical = RetweetHandler::build_canonical_chat(&message);

            let openid = message
                .author
                .user_openid
                .as_deref()
                .unwrap_or(&message.author.id);

            match self
                .dm_client
                .reply_message(openid, message.id.clone(), "收到".to_string(), None)
                .await
            {
                Ok(response) => {
                    info!(
                        message_id = %response.id,
                        openid = %openid,
                        "回复消息成功"
                    );
                }
                Err(e) => {
                    error!(error = %e, openid = %openid, "回复消息失败");
                }
            }

            match self.redis.publish_canonical_chat(&canonical).await {
                Ok(_) => {
                    info!(
                        id = %canonical.id,
                        dedup_key = %canonical.dedup_key,
                        "QQ canonical CHAT 发布成功"
                    );
                }
                Err(e) => {
                    error!(error = %e, id = %canonical.id, "QQ canonical CHAT 发布失败");
                }
            }
        })
    }
}

/// 平台 timestamp → protobuf Timestamp。
///
/// 优先 RFC3339（QQ C2C 实测样例 `2026-08-26T19:25:43+08:00`，见 qq-bot
/// websocket.rs 样例）；兼容纯数字秒/毫秒（canonical-msg ChatMessage.received_at
/// 注释约定"秒/毫秒由 A 解析"）；解析失败返回 `None`（调用方回退当前时间）。
fn parse_platform_timestamp(raw: &str) -> Option<pbjson_types::Timestamp> {
    if let Ok(ts) = jiff::Timestamp::from_str(raw) {
        return Some(pbjson_types::Timestamp {
            seconds: ts.as_second(),
            nanos: ts.subsec_nanosecond(),
        });
    }
    let numeric = raw.trim().parse::<i64>().ok()?;
    let (seconds, nanos) = if numeric.unsigned_abs() >= 1_000_000_000_000 {
        // 13 位 = 毫秒。
        (numeric / 1000, ((numeric % 1000) * 1_000_000) as i32)
    } else {
        // 10 位 = 秒。
        (numeric, 0)
    };
    if seconds < 0 {
        return None;
    }
    Some(pbjson_types::Timestamp { seconds, nanos })
}

fn now_timestamp() -> pbjson_types::Timestamp {
    let now = jiff::Timestamp::now();
    pbjson_types::Timestamp {
        seconds: now.as_second(),
        nanos: now.subsec_nanosecond(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use canonical_msg::MsgKind;
    use qq_bot::MessageAuthor;

    /// QQ C2C 事件样例（字段对齐 qq-bot websocket.rs `parses_minimal_c2c_event_from_official_docs`）。
    fn fake_event(id: &str, author: MessageAuthor, content: &str, timestamp: &str) -> MessageEvent {
        MessageEvent {
            id: id.to_string(),
            author,
            content: content.to_string(),
            message_type: 0,
            timestamp: timestamp.to_string(),
            message_scene: None,
        }
    }

    fn author_with_openid(openid: &str) -> MessageAuthor {
        MessageAuthor {
            id: "u-1001".to_string(),
            username: None,
            bot: false,
            user_openid: Some(openid.to_string()),
            union_openid: None,
        }
    }

    #[test]
    fn build_chat_canonical_fields_from_qq_event() {
        let event = fake_event(
            "pm-0001",
            author_with_openid("o-1001"),
            "你好，请问今天更新了吗？",
            "2026-08-26T19:25:43+08:00",
        );
        let msg = RetweetHandler::build_canonical_chat(&event);

        // 顶层：kind=CHAT、source=IM、ULID id、dedup_key 格式。
        assert_eq!(msg.kind, MsgKind::Chat as i32);
        let source = msg.source.unwrap();
        assert_eq!(source.kind, SourceKind::Im as i32);
        assert_eq!(source.id, "qq-c2c");
        assert_eq!(source.display_name, "QQ 私聊");
        assert!(!msg.id.is_empty(), "id 必须是 A 层生成的 ULID");
        assert_eq!(msg.dedup_key, "qq-c2c:pm-0001");

        // 正文约定：body_text = body_markdown = content 原文；title/links/tags 空。
        assert_eq!(msg.body_text, "你好，请问今天更新了吗？");
        assert_eq!(msg.body_markdown, msg.body_text);
        assert!(msg.title.is_empty());
        assert!(msg.links.is_empty());
        assert!(msg.tags.is_empty());

        // 时间：published_at = received_at 同值，RFC3339+08:00 正确换算 UTC。
        let ts = msg.published_at.unwrap();
        assert_eq!((ts.seconds, ts.nanos), (1_787_743_543, 0));

        // domain.chat 各字段。
        let chat = match msg.domain.unwrap() {
            canonical_message::Domain::Chat(chat) => chat,
            _ => panic!("expected chat domain"),
        };
        assert_eq!(chat.platform_message_id, "pm-0001");
        assert_eq!(chat.author_id, "u-1001");
        assert_eq!(chat.author_openid, "o-1001");
        assert_eq!(chat.content, "你好，请问今天更新了吗？");
        assert_eq!(chat.received_at, Some(ts));
        assert!(chat.chat_id.is_empty(), "QQ C2C：chat_id 留空");
    }

    #[test]
    fn build_chat_handles_minimal_c2c_event_without_author_id() {
        // 官方样例：author 仅 user_openid，无 id/bot/message_type 字段。
        let event = fake_event(
            "ROBOT1.0_.b6nx",
            MessageAuthor {
                id: String::new(),
                username: None,
                bot: false,
                user_openid: Some("E4F4AEA33253A2797FB897C50B81D7ED".to_string()),
                union_openid: None,
            },
            "123",
            "2023-11-06T13:37:18+08:00",
        );
        let msg = RetweetHandler::build_canonical_chat(&event);
        let chat = match msg.domain.unwrap() {
            canonical_message::Domain::Chat(chat) => chat,
            _ => panic!("expected chat domain"),
        };
        assert_eq!(chat.platform_message_id, "ROBOT1.0_.b6nx");
        assert_eq!(chat.author_id, "");
        assert_eq!(chat.author_openid, "E4F4AEA33253A2797FB897C50B81D7ED");
        assert_eq!(msg.dedup_key, "qq-c2c:ROBOT1.0_.b6nx");
    }

    #[test]
    fn published_at_falls_back_to_now_on_unparseable_timestamp() {
        let event = fake_event("pm-0002", author_with_openid("o-1002"), "hi", "not-a-time");
        let msg = RetweetHandler::build_canonical_chat(&event);
        let ts = msg.published_at.unwrap();
        let now = jiff::Timestamp::now();
        assert!(
            (now.as_second() - ts.seconds).abs() <= 60,
            "解析失败应回退当前时间（±60s），实际 {} vs {}",
            ts.seconds,
            now.as_second()
        );
        let chat = match msg.domain.unwrap() {
            canonical_message::Domain::Chat(chat) => chat,
            _ => panic!("expected chat domain"),
        };
        assert_eq!(
            chat.received_at,
            Some(ts),
            "published_at 与 received_at 同值"
        );
    }

    #[test]
    fn published_at_accepts_numeric_seconds_and_millis() {
        let event = fake_event("pm-0003", author_with_openid("o-1003"), "hi", "1787743543");
        let msg = RetweetHandler::build_canonical_chat(&event);
        assert_eq!(msg.published_at.unwrap().seconds, 1_787_743_543);

        let event = fake_event(
            "pm-0004",
            author_with_openid("o-1004"),
            "hi",
            "1787743543123",
        );
        let msg = RetweetHandler::build_canonical_chat(&event);
        let ts = msg.published_at.unwrap();
        assert_eq!((ts.seconds, ts.nanos), (1_787_743_543, 123_000_000));
    }
}
