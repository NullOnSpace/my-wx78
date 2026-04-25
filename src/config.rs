use std::env;
use tracing::{debug, info};

#[derive(Clone)]
pub struct AppConfig {
    pub qq_bot: QqBotConfig,
    pub redis: RedisConfig,
}

#[derive(Clone)]
pub struct QqBotConfig {
    pub app_id: String,
    pub client_secret: String,
    pub open_id: String,
}

#[derive(Clone)]
pub struct RedisConfig {
    pub url: String,
    pub source_channels: Vec<String>,
    pub retweet_channel: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, env::VarError> {
        debug!("从环境变量加载配置");

        let qq_bot = QqBotConfig::from_env()?;
        let redis = RedisConfig::from_env()?;

        info!(
            app_id = %qq_bot.app_id,
            open_id = %qq_bot.open_id,
            redis_url = %redis.url,
            source_channels = ?redis.source_channels,
            retweet_channel = %redis.retweet_channel,
            "成功加载配置"
        );

        Ok(Self { qq_bot, redis })
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
        let source_channels = env::var("REDIS_SOURCE_CHANNELS")
            .unwrap_or_else(|_| "qqbot".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let retweet_channel =
            env::var("REDIS_RETWEET_CHANNEL").unwrap_or_else(|_| "qqbot_retweet".to_string());

        Ok(Self {
            url,
            source_channels,
            retweet_channel,
        })
    }

    pub fn connection_url(&self) -> String {
        self.url.clone()
    }
}
