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
    #[serde(default)]
    pub nightbot: NightbotConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NightbotConfig {
    /// OAuth Authorization Code Flow のコールバックを受けるローカルポート。既定 8123。
    /// 変更時は Nightbot OAuth アプリの Redirect URI も合わせること。
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,
}

// `#[derive(Default)]` を使うと u16::default() = 0 になり、[nightbot] セクションを丸ごと省略した
// 場合に Config 側の #[serde(default)] が NightbotConfig::default() を呼んで callback_port=0 に
// なってしまう。手書きで既定値を返す。
impl Default for NightbotConfig {
    fn default() -> Self {
        Self {
            callback_port: default_callback_port(),
        }
    }
}

fn default_callback_port() -> u16 {
    8123
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
        config.validate()?;
        Ok(config)
    }

    /// callback_port = 0 のような明示的に不正な設定値を弾く。
    /// Server::http(("127.0.0.1", 0)) は OS の ephemeral port を割り当てるため、
    /// 認可 URL が http://127.0.0.1:0/callback になり Nightbot OAuth アプリの
    /// Redirect URI とミスマッチで認証が通らなくなる。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.nightbot.callback_port == 0 {
            return Err(ConfigError::InvalidCallbackPort);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイル読み込み失敗: {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("TOML パース失敗: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("[nightbot] callback_port に 0 を指定できません (Nightbot OAuth アプリの Redirect URI とミスマッチするため)")]
    InvalidCallbackPort,
}
