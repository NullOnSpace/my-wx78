mod config;
mod handler;
mod redis;

use config::AppConfig;
use handler::RetweetHandler;
use qq_bot::{AuthClient, AuthConfig, DirectMessageClient, WebSocketClient, WebSocketConfig};
use redis::RedisManager;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "my_wx78=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv()?;

    let app_config = AppConfig::from_env()?;
    let redis_config = app_config.redis.clone();
    let qq_bot_config = app_config.qq_bot.clone();

    let redis_manager = RedisManager::new(redis_config.clone()).await?;
    let redis = Arc::new(Mutex::new(redis_manager));

    let auth_config = AuthConfig {
        app_id: qq_bot_config.app_id,
        client_secret: qq_bot_config.client_secret,
    };
    let auth_client = AuthClient::new(auth_config);
    let access_token = auth_client.get_access_token().await?;
    info!("成功获取QQ Bot access_token");

    let ws_task = start_websocket(access_token.clone(), Arc::clone(&redis));

    let dm_task = start_redis_subscriber(redis_config, access_token, qq_bot_config.open_id);

    tokio::select! {
        result = ws_task => {
            error!(result = ?result, "WebSocket任务结束");
        }
        result = dm_task => {
            error!(result = ?result, "Redis订阅任务结束");
        }
    }

    Ok(())
}

async fn start_websocket(
    access_token: String,
    redis: Arc<Mutex<RedisManager>>,
) -> Result<(), qq_bot::WebSocketError> {
    info!("启动WebSocket连接，监听QQ消息");
    let handler = Arc::new(RetweetHandler::new(redis));
    let ws_config = WebSocketConfig::new(access_token, 2113934851);
    let ws_client = Arc::new(WebSocketClient::new(ws_config));
    ws_client.connect(handler).await
}

async fn start_redis_subscriber(
    redis_config: config::RedisConfig,
    access_token: String,
    open_id: String,
) -> Result<(), redis::RedisError> {
    info!(channels = ?redis_config.source_channels, "启动Redis订阅，监听外部消息");

    let dm_client = DirectMessageClient::new(access_token);
    let dm_client = Arc::new(dm_client);
    let open_id = Arc::new(open_id);

    let redis_manager = RedisManager::new(redis_config.clone()).await?;
    redis_manager
        .subscribe_source_channels(move |_channel, payload| {
            let dm_client = Arc::clone(&dm_client);
            let open_id = Arc::clone(&open_id);
            tokio::spawn(async move {
                info!(
                    payload_len = payload.len(),
                    "从Redis收到消息，准备发送给QQ用户"
                );
                match dm_client.send_text(&open_id, payload).await {
                    Ok(response) => {
                        info!(
                            message_id = %response.id,
                            "消息发送给QQ用户成功"
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "消息发送给QQ用户失败");
                    }
                }
            });
        })
        .await
}
