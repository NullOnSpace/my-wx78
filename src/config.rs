use std::env;
use tracing::{debug, info};

#[derive(Clone)]
pub struct AppConfig {
    pub qq_bot: QqBotConfig,
    pub redis: RedisConfig,
    pub outbound: OutboundConfig,
}

#[derive(Clone)]
pub struct QqBotConfig {
    pub app_id: String,
    pub client_secret: String,
    /// 入向回复兜底 openid：入向事件缺 user_openid 时 handler 用它回"收到"。
    /// 不是出向投递目标（出向以 RenderEnvelope.target_id 为准，见 outbound.rs）。
    pub open_id: String,
}

#[derive(Clone)]
pub struct RedisConfig {
    pub url: String,
    /// 入向 canonical CHAT 的 XADD 目标 stream（`stream:msgs:in`，push-core 消费）。
    pub msgs_stream: String,
    /// 出向消费的渲染信封 stream（`stream:rendered:qq`，render-hub C 层产出）。
    pub rendered_stream: String,
    /// 发送回执的 XADD 目标 stream（`stream:receipts`，push-core 消费）。
    pub receipts_stream: String,
}

#[derive(Clone)]
pub struct OutboundConfig {
    /// QQ 主动消息限流（令牌桶回填速率，个/分钟；突发容量同值）。
    pub rate_limit_per_min: u32,
    /// 单次平台发送的最大尝试次数（含首次，dispatch-core RetryPolicy）。
    pub retry_max_attempts: u32,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, env::VarError> {
        debug!("从环境变量加载配置");

        let qq_bot = QqBotConfig::from_env()?;
        let redis = RedisConfig::from_env()?;
        let outbound = OutboundConfig::from_env();

        info!(
            app_id = %qq_bot.app_id,
            redis_url = %redis.url,
            msgs_stream = %redis.msgs_stream,
            rendered_stream = %redis.rendered_stream,
            receipts_stream = %redis.receipts_stream,
            rate_limit_per_min = outbound.rate_limit_per_min,
            retry_max_attempts = outbound.retry_max_attempts,
            "成功加载配置"
        );

        Ok(Self {
            qq_bot,
            redis,
            outbound,
        })
    }
}

impl QqBotConfig {
    fn from_env() -> Result<Self, env::VarError> {
        let app_id = env::var("QQ_BOT_APP_ID")?;
        let client_secret = env::var("QQ_BOT_CLIENT_SECRET")?;
        let open_id = env::var("QQ_OPEN_ID")?;
        Ok(Self {
            app_id,
            client_secret,
            open_id,
        })
    }
}

impl RedisConfig {
    fn from_env() -> Result<Self, env::VarError> {
        let url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
        let msgs_stream =
            env::var("REDIS_MSGS_STREAM").unwrap_or_else(|_| "stream:msgs:in".to_string());
        let rendered_stream =
            env::var("REDIS_RENDERED_STREAM").unwrap_or_else(|_| "stream:rendered:qq".to_string());
        let receipts_stream =
            env::var("REDIS_RECEIPTS_STREAM").unwrap_or_else(|_| "stream:receipts".to_string());

        Ok(Self {
            url,
            msgs_stream,
            rendered_stream,
            receipts_stream,
        })
    }

    pub fn connection_url(&self) -> String {
        self.url.clone()
    }
}

impl OutboundConfig {
    fn from_env() -> Self {
        let rate_limit_per_min = env::var("QQ_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(30);
        let retry_max_attempts = env::var("RETRY_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(3);

        Self {
            rate_limit_per_min,
            retry_max_attempts,
        }
    }
}
