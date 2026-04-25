mod config;
mod handler;
mod redis;

use config::AppConfig;
use handler::RetweetHandler;
use qq_bot::{AuthClient, AuthConfig, DirectMessageClient, WebSocketClient, WebSocketConfig};
use redis::RedisManager;
use reqwest::Client;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(1024 * 1024)
        .enable_all()
        .build()?;

    runtime.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "my_wx78=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv()?;

    let app_config = AppConfig::from_env()?;
    let redis_config = app_config.redis;
    let qq_bot_config = app_config.qq_bot;

    let redis_manager = RedisManager::new(&redis_config).await?;
    let redis = Arc::new(redis_manager);

    let http_client = Arc::new(Client::new());

    let auth_config = AuthConfig {
        app_id: qq_bot_config.app_id,
        client_secret: qq_bot_config.client_secret,
    };
    let auth_client = AuthClient::new(auth_config, Arc::clone(&http_client));
    let access_token = auth_client.get_access_token().await?;
    info!("成功获取QQ Bot access_token");

    let dm_client = Arc::new(DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&access_token)));
    let open_id = Arc::new(qq_bot_config.open_id);

    let ws_task = start_websocket(access_token, Arc::clone(&http_client), Arc::clone(&redis), Arc::clone(&dm_client));

    let dm_task = start_redis_subscriber(Arc::clone(&redis), dm_client, open_id);

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
    access_token: Arc<str>,
    http_client: Arc<Client>,
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
) -> Result<(), qq_bot::WebSocketError> {
    info!("启动WebSocket连接，监听QQ消息");
    let handler = Arc::new(RetweetHandler::new(redis, dm_client));
    let ws_config = WebSocketConfig::new(access_token, 2113934851);
    let ws_client = Arc::new(WebSocketClient::new(ws_config, http_client));
    ws_client.connect(handler).await
}

async fn start_redis_subscriber(
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
    open_id: Arc<String>,
) -> Result<(), redis::RedisError> {
    info!("启动Redis订阅，监听外部消息");

    redis
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