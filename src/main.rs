mod config;
mod handler;
mod outbound;
mod redis;

use anyhow::Context;
use config::AppConfig;
use handler::RetweetHandler;
use qq_bot::{AuthClient, AuthConfig, DirectMessageClient, WebSocketClient, WebSocketConfig};
use redis::RedisManager;
use reqwest::Client;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(1024 * 1024)
        .enable_all()
        .build()?;

    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "my_wx78=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv().ok();

    let app_config = AppConfig::from_env()?;

    let http_client = Arc::new(Client::new());

    let auth_config = AuthConfig {
        app_id: app_config.qq_bot.app_id.clone().into(),
        client_secret: app_config.qq_bot.client_secret.clone().into(),
    };
    let auth_client = Arc::new(AuthClient::new(auth_config, Arc::clone(&http_client)));

    let dm_client = Arc::new(DirectMessageClient::new(
        Arc::clone(&http_client),
        Arc::clone(&auth_client),
    ));

    let redis = Arc::new(RedisManager::new(&app_config.redis).await?);

    let cancellation_token = CancellationToken::new();

    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();

    // 任务 1：入向链（QQ WS → RetweetHandler → canonical CHAT XADD stream:msgs:in + 回复"收到"）。
    tasks.spawn(start_websocket(
        Arc::clone(&auth_client),
        Arc::clone(&http_client),
        Arc::clone(&redis),
        Arc::clone(&dm_client),
        cancellation_token.clone(),
    ));

    // 任务 2：出向链（stream:rendered:qq 消费 → QQ 发送 → receipts XADD stream:receipts）。
    tasks.spawn(outbound::run_outbound(
        app_config.clone(),
        Arc::clone(&dm_client),
        cancellation_token.clone(),
    ));

    shutdown_signal().await?;
    info!("收到关机信号，开始优雅关闭");
    cancellation_token.cancel();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(error = %e, "任务异常结束"),
            Err(e) => error!(error = %e, "任务panic或被中止"),
        }
    }

    Ok(())
}

async fn start_websocket(
    auth_client: Arc<AuthClient>,
    http_client: Arc<Client>,
    redis: Arc<RedisManager>,
    dm_client: Arc<DirectMessageClient>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("启动WebSocket连接，监听QQ消息");
    let handler = Arc::new(RetweetHandler::new(redis, dm_client));
    let ws_config = WebSocketConfig::new(2113934851);
    let ws_client = Arc::new(WebSocketClient::new(auth_client, ws_config, http_client));
    ws_client
        .run(handler, cancellation_token)
        .await
        .context("WebSocket任务异常结束")
}

async fn shutdown_signal() -> anyhow::Result<()> {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| anyhow::anyhow!("ctrl_c handler failed: {error}"))
    };
    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|error| anyhow::anyhow!("SIGTERM handler install failed: {error}"))?;
        sigterm.recv().await;
        Ok::<(), anyhow::Error>(())
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        result = ctrl_c => { result? }
        result = terminate => { result? }
    }
    Ok(())
}
