use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    #[serde(default)]
    pub twitch: Option<TwitchCreds>,
    #[serde(default)]
    pub youtube: Option<YoutubeCreds>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TwitchCreds {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64, // unix epoch seconds
    pub client_id: String,
    /// 取得済みなら Helix で得た自分のログイン名をキャッシュ (任意)
    #[serde(default)]
    pub login_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct YoutubeCreds {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub client_id: String,
    pub client_secret: String,
}

impl Credentials {
    /// credentials.toml のパス: $XDG_CONFIG_HOME/ow2-victory-notifier/credentials.toml
    /// (Linux), %APPDATA%\ow2-victory-notifier\credentials.toml (Windows),
    /// ~/Library/Application Support/ow2-victory-notifier/credentials.toml (macOS)
    pub fn path() -> Result<PathBuf, CredentialsError> {
        let config_dir = dirs::config_dir().ok_or(CredentialsError::NoConfigDir)?;
        Ok(config_dir.join("ow2-victory-notifier").join("credentials.toml"))
    }

    /// ファイルから読み込み。存在しない場合は空の Credentials を返す
    pub fn load() -> Result<Self, CredentialsError> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| CredentialsError::Io(path.display().to_string(), e))?;
        let creds: Credentials = toml::from_str(&content)?;
        Ok(creds)
    }

    /// ファイルへ保存。ディレクトリは自動生成、Unix では 0o600 に設定
    pub fn save(&self) -> Result<(), CredentialsError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CredentialsError::Io(parent.display().to_string(), e))?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content.as_bytes())
            .map_err(|e| CredentialsError::Io(path.display().to_string(), e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| CredentialsError::Io(path.display().to_string(), e))?;
        }
        tracing::debug!("credentials saved to {}", path.display());
        Ok(())
    }

    pub fn set_twitch(&mut self, creds: TwitchCreds) {
        self.twitch = Some(creds);
    }

    pub fn set_youtube(&mut self, creds: YoutubeCreds) {
        self.youtube = Some(creds);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("OS の設定ディレクトリーを取得できませんでした")]
    NoConfigDir,
    #[error("ファイル I/O エラー: {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("TOML パース失敗: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("TOML シリアライズ失敗: {0}")]
    TomlSer(#[from] toml::ser::Error),
}
