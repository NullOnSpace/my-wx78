mod config;
mod handler;
mod redis;

use config::AppConfig;
use handler::RetweetHandler;
use qq_bot::{AuthClient, AuthConfig, CancellationToken, DirectMessageClient, WebSocketClient, WebSocketConfig};
use redis::RedisManager;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const MAX_REDIS_BACKOFF_SECS: u64 = 30;

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

    let http_client = Arc::new(Client::new());

    let auth_config = AuthConfig {
        app_id: qq_bot_config.app_id.into(),
        client_secret: qq_bot_config.client_secret.into(),
    };
    let auth_client = Arc::new(AuthClient::new(auth_config, Arc::clone(&http_client)));

    let dm_client = Arc::new(DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&auth_client)));

    let redis = Arc::new(RedisManager::new(&redis_config).await?);

    let cancellation_token = CancellationToken::new();

    let ws_task = tokio::spawn(start_websocket(
        Arc::clone(&auth_client),
        Arc::clone(&http_client),
        Arc::clone(&redis),
        Arc::clone(&dm_client),
        cancellation_token.clone(),
    ));

    let redis_task = tokio::spawn(start_redis_subscriber(
        redis_config,
        Arc::clone(&redis),
        Arc::clone(&dm_client),
        Arc::new(qq_bot_config.open_id),
        cancellation_token.clone(),
    ));

    tokio::select! {
        result = ws_task => {
            match result {
                Ok(Ok(())) => info!("WebSocket任务正常结束"),
                Ok(Err(e)) => error!(error = %e, "WebSocket任务异常结束"),
                Err(e) => error!(error = %e, "WebSocket任务panic"),
            }
        }
        result = redis_task => {
            match result {
                Ok(Ok(())) => info!("Redis订阅任务正常结束"),
                Ok(Err(e)) => error!(error = %e, "Redis订阅任务异常结束"),
                Err(e) => error!(error = %e, "Redis订阅任务panic"),
            }
        }
    }

    cancellation_token.cancel();
    Ok(())
}

async fn start_websocket(
    auth_client: Arc<AuthClient>,
    http_client: Arc<Client>,
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
    cancellation_token: CancellationToken,
) -> Result<(), qq_bot::WebSocketError> {
    info!("启动WebSocket连接，监听QQ消息");
    let handler = Arc::new(RetweetHandler::new(redis, dm_client));
    let ws_config = WebSocketConfig::new(2113934851);
    let ws_client = Arc::new(WebSocketClient::new(auth_client, ws_config, http_client));
    ws_client.run(handler, cancellation_token).await
}

async fn start_redis_subscriber(
    redis_config: config::RedisConfig,
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
    open_id: Arc<String>,
    cancellation_token: CancellationToken,
) -> Result<(), redis::RedisError> {
    info!("启动Redis订阅，监听外部消息");
    let mut backoff_secs: u64 = 1;

    loop {
        let result = redis
            .subscribe_source_channels(
                &redis_config,
                {
                    let dm_client = Arc::clone(&dm_client);
                    let open_id = Arc::clone(&open_id);
                    move |_channel, payload| {
                        let dm_client = Arc::clone(&dm_client);
                        let open_id = Arc::clone(&open_id);
                        tokio::spawn(async move {
                            info!(payload_len = payload.len(), "从Redis收到消息，准备发送给QQ用户");
                            match dm_client.send_text(&open_id, payload).await {
                                Ok(response) => {
                                    info!(message_id = %response.id, "消息发送给QQ用户成功");
                                }
                                Err(e) => {
                                    error!(error = %e, "消息发送给QQ用户失败");
                                }
                            }
                        });
                    }
                },
            )
            .await;

        match result {
            Ok(()) => {
                info!("Redis订阅正常断开，准备重连");
                backoff_secs = 1;
            }
            Err(e) => {
                error!(error = %e, "Redis订阅异常断开");
                backoff_secs = (backoff_secs * 2).min(MAX_REDIS_BACKOFF_SECS);
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {
                info!(backoff_secs, "等待后重连Redis订阅");
            }
            _ = cancellation_token.cancelled() => {
                info!("收到关机信号，停止Redis订阅重连");
                return Ok(());
            }
        }
    }
}