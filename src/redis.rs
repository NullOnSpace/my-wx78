pub use redis::RedisError;

use crate::config::RedisConfig;
use futures_util::StreamExt;
use redis::aio::MultiplexedConnection;
use redis::{Client, ErrorKind, Value};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

#[derive(Debug, Serialize, Deserialize)]
pub struct RetweetMessage {
    pub message_id: String,
    pub author_id: String,
    pub author_openid: Option<String>,
    pub content: String,
    pub timestamp: String,
}

pub struct RedisManager {
    config: RedisConfig,
    connection: MultiplexedConnection,
}

impl RedisManager {
    pub async fn new(config: &RedisConfig) -> Result<Self, RedisError> {
        let connection = Self::create_connection(&config).await?;
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

    pub async fn publish_retweet(&self, message: &RetweetMessage) -> Result<(), RedisError> {
        let json = serde_json::to_string(message).map_err(|e| {
            error!(error = %e, "序列化转发消息失败");
            RedisError::from((ErrorKind::Client, "序列化转发消息失败"))
        })?;

        let channel = &self.config.retweet_channel;
        let result: Value = redis::cmd("PUBLISH")
            .arg(channel)
            .arg(&json)
            .query_async(&mut self.connection.clone())
            .await?;

        match result {
            Value::Int(n) => {
                info!(channel = %channel, receivers = n, "转发消息发布成功");
                Ok(())
            }
            _ => Err(RedisError::from((
                ErrorKind::Client,
                "PUBLISH返回非整数结果",
            ))),
        }
    }

    pub async fn subscribe_source_channels(
        &self,
        config: &RedisConfig,
        handler: impl Fn(String, String) + Send + 'static,
    ) -> Result<(), RedisError> {
        let channels = &config.source_channels;
        if channels.is_empty() {
            warn!("没有配置订阅频道，跳过订阅");
            return Ok(());
        }

        info!(channels = ?channels, "开始订阅Redis频道");

        let client = Client::open(config.connection_url())?;
        let mut pubsub_conn = client.get_async_pubsub().await?;

        for channel in channels {
            pubsub_conn.subscribe(channel).await?;
            info!(channel = %channel, "已订阅频道");
        }

        let mut msg_stream = pubsub_conn.on_message();

        while let Some(msg) = msg_stream.next().await {
            let channel_name = msg.get_channel_name().to_string();
            let payload: String = msg.get_payload().unwrap_or_else(|e| {
                error!(error = %e, channel = %channel_name, "解析消息payload失败");
                String::new()
            });
            if !payload.is_empty() {
                info!(channel = %channel_name, payload_len = payload.len(), "收到订阅消息");
                handler(channel_name, payload);
            }
        }

        warn!("订阅连接断开");
        Ok(())
    }
}