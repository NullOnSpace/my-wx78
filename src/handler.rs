use crate::redis::{RedisManager, RetweetMessage};
use async_trait::async_trait;
use qq_bot::{DirectMessageClient, MessageEvent, WebSocketClient, WebSocketHandler};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

pub struct RetweetHandler {
    redis: Arc<Mutex<RedisManager>>,
}

impl RetweetHandler {
    pub fn new(redis: Arc<Mutex<RedisManager>>) -> Self {
        Self { redis }
    }
}

#[async_trait]
impl WebSocketHandler for RetweetHandler {
    async fn on_message(&self, message: MessageEvent, client: Arc<WebSocketClient>) {
        info!(
            message_id = %message.id,
            author_id = %message.author.id,
            content = %message.content,
            "收到QQ消息，准备转发到Redis"
        );

        let openid = message
            .author
            .user_openid
            .clone()
            .unwrap_or_else(|| message.author.id.clone());

        let dm_client = DirectMessageClient::new(client.token().to_string());
        match dm_client
            .reply_message(&openid, message.id.clone(), "收到".to_string(), None)
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

        let retweet = RetweetMessage {
            message_id: message.id,
            author_id: message.author.id,
            author_openid: message.author.user_openid,
            content: message.content,
            timestamp: message.timestamp,
        };

        let mut redis = self.redis.lock().await;
        match redis.publish_retweet(&retweet).await {
            Ok(_) => {
                info!(message_id = %retweet.message_id, "QQ消息转发成功");
            }
            Err(e) => {
                error!(error = %e, message_id = %retweet.message_id, "QQ消息转发失败");
            }
        }
    }
}
