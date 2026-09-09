pub use redis::RedisError;

use canonical_msg::CanonicalMessage;
use canonical_msg::prost::Message;
use redis::Client;
use redis::aio::MultiplexedConnection;
use streams_lib::xadd_message;
use tracing::{error, info};

use crate::config::RedisConfig;

pub struct RedisManager {
    config: RedisConfig,
    connection: MultiplexedConnection,
}

impl RedisManager {
    pub async fn new(config: &RedisConfig) -> Result<Self, RedisError> {
        let connection = Self::create_connection(config).await?;
        Ok(Self {
            config: config.clone(),
            connection,
        })
    }

    async fn create_connection(config: &RedisConfig) -> Result<MultiplexedConnection, RedisError> {
        let client = Client::open(config.connection_url())?;
        let connection = client.get_multiplexed_async_connection().await?;
        info!(url = %config.connection_url(), "Redis连接成功");
        Ok(connection)
    }

    /// 入向 canonical CHAT 发布：proto 编码后 XADD 到 `stream:msgs:in`。
    ///
    /// 语义与 pub/sub PUBLISH 对齐（fire-and-forget，XADD 成功即认为已投）；
    /// 可靠投递由 push-core 消费侧 consumer group pending 兜底。
    pub async fn publish_canonical_chat(
        &self,
        message: &CanonicalMessage,
    ) -> Result<(), RedisError> {
        let bytes = message.encode_to_vec();
        let stream = &self.config.msgs_stream;

        let id = xadd_message(&mut self.connection.clone(), stream, &bytes)
            .await
            .map_err(|e| {
                error!(
                    error = %e,
                    stream = %stream,
                    message_id = %message.id,
                    "XADD canonical chat 失败"
                );
                e
            })?;

        info!(
            stream = %stream,
            id = %id,
            message_id = %message.id,
            dedup_key = %message.dedup_key,
            "canonical chat 消息发布成功"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::handler::RetweetHandler;
    use canonical_msg::canonical_message;
    use canonical_msg::{MsgKind, SourceKind};
    use qq_bot::{MessageAuthor, MessageEvent};
    use redis::AsyncCommands;
    use redis::streams::StreamRangeReply;

    fn fake_event() -> MessageEvent {
        MessageEvent {
            id: "pm-0001".to_string(),
            author: MessageAuthor {
                id: "u-1001".to_string(),
                username: None,
                bot: false,
                user_openid: Some("o-1001".to_string()),
                union_openid: None,
            },
            content: "你好，请问今天更新了吗？".to_string(),
            message_type: 0,
            timestamp: "2026-08-26T19:25:43+08:00".to_string(),
            message_scene: None,
        }
    }

    /// 入向集成（真实 Redis）：mock WS 事件 → publish_canonical_chat → XRANGE
    /// `stream:msgs:in` 断言 CHAT canonical proto 可 decode 且字段正确。
    /// env 门控：`MY_WX78_TEST_REDIS=1`。
    #[tokio::test]
    async fn inbound_publishes_chat_canonical_to_msgs_stream_when_env_configured() {
        if std::env::var("MY_WX78_TEST_REDIS").as_deref() != Ok("1") {
            eprintln!("skipping: MY_WX78_TEST_REDIS != 1");
            return;
        }
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let stream = "my-wx78-test-msgs-in";
        let client = redis::Client::open(url.as_str()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        // 清理测试残留，保证 XRANGE 只有本测试的一条。
        redis::cmd("DEL")
            .arg(stream)
            .query_async::<i64>(&mut conn)
            .await
            .unwrap();

        let config = RedisConfig {
            url,
            msgs_stream: stream.to_string(),
            rendered_stream: "my-wx78-test-rendered".to_string(),
            receipts_stream: "my-wx78-test-receipts".to_string(),
        };
        let manager = RedisManager::new(&config).await.unwrap();

        let canonical = RetweetHandler::build_canonical_chat(&fake_event());
        manager.publish_canonical_chat(&canonical).await.unwrap();

        let reply: StreamRangeReply = conn.xrange_all(stream).await.unwrap();
        assert_eq!(reply.ids.len(), 1, "stream 应恰好一条 canonical");
        let data: Vec<u8> = reply.ids[0].get("data").expect("entry 应携带 data 字段");
        let decoded = CanonicalMessage::decode(data.as_slice()).unwrap();

        assert_eq!(decoded.kind, MsgKind::Chat as i32);
        assert_eq!(decoded.dedup_key, "qq-c2c:pm-0001");
        assert_eq!(decoded.source.unwrap().kind, SourceKind::Im as i32);
        assert_eq!(decoded.body_text, "你好，请问今天更新了吗？");
        match decoded.domain.unwrap() {
            canonical_message::Domain::Chat(chat) => {
                assert_eq!(chat.platform_message_id, "pm-0001");
                assert_eq!(chat.author_openid, "o-1001");
                assert_eq!(chat.content, "你好，请问今天更新了吗？");
                assert!(chat.chat_id.is_empty());
            }
            _ => panic!("expected chat domain"),
        }
    }
}
