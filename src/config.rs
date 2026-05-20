use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub detector: DetectorConfig,
    pub platforms: PlatformsConfig,
    pub twitch: TwitchConfig,
    pub youtube: YoutubeConfig,
    pub messages: MessagesConfig,
    pub filter: FilterConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DetectorConfig {
    pub sse_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlatformsConfig {
    pub twitch_enabled: bool,
    pub youtube_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TwitchConfig {
    pub channel: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct YoutubeConfig {
    #[serde(default)]
    pub video_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesConfig {
    pub language: String, // "ja" or "en"
    pub victory: String,
    pub defeat: String,
    pub draw: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterConfig {
    pub auto_only: bool,
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ConfigError::Io(path.as_ref().display().to_string(), e))?;
        let config: Config = toml::from_str(&content).map_err(ConfigError::Toml)?;
        Ok(config)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイル読み込み失敗: {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("TOML パース失敗: {0}")]
    Toml(#[from] toml::de::Error),
}
