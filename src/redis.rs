pub use redis::RedisError;

use crate::config::RedisConfig;
use redis::aio::MultiplexedConnection;
use redis::{Client, ErrorKind, Value};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

/// 入向消息（QQ WS → Redis `qqbot-retweet`）。Phase 5 才 canonical 化。
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
}
