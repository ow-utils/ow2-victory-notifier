use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    #[serde(default)]
    pub nightbot: Option<NightbotCreds>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NightbotCreds {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch 秒。
    pub expires_at: u64,
    pub client_id: String,
    pub client_secret: String,
    /// authenticate 時に使った callback_port。refresh 時の redirect_uri は authorize 時と
    /// 完全一致が必須 (Nightbot 仕様) のため、credentials 側に保存して config の事後変更から
    /// 独立させる。
    pub callback_port: u16,
}

/// 同一 account の二重起動を防ぐ advisory lock の RAII guard。Drop で OS が flock を解放する。
#[derive(Debug)]
pub struct CredentialsLock {
    /// ファイルハンドル保持自体が目的 (drop で flock が解放される)。
    #[allow(dead_code)]
    file: std::fs::File,
}

impl Credentials {
    fn config_dir() -> Result<PathBuf, CredentialsError> {
        let d = dirs::config_dir().ok_or(CredentialsError::NoConfigDir)?;
        Ok(d.join("ow2-victory-notifier"))
    }

    fn file_name(account: &str) -> String {
        format!("credentials-{account}.toml")
    }

    #[allow(dead_code)]
    pub fn path(account: &str) -> Result<PathBuf, CredentialsError> {
        Ok(Self::config_dir()?.join(Self::file_name(account)))
    }

    fn lock_path_in(dir: &Path, account: &str) -> PathBuf {
        // lock ファイル本体はプロセス終了後も意図的に残置する。`remove_file` を unlock の
        // 前に挟むと別プロセスが新規 open 中に inode が差し変わり、両者が「自分が排他取得した」
        // と思い込む race の元になる。OS の flock はファイル消失で自動解放されないため、
        // 残ったファイルがあっても 2 回目以降の `try_lock_exclusive` は正しく挙動する。
        dir.join(format!("credentials-{account}.lock"))
    }

    /// account 単位の sidecar lock を取得しつつ credentials ファイルを読み込む。
    /// CredentialsLock を関数終了まで保持しないと flock が解放されるので注意。
    pub fn load_locked(account: &str) -> Result<(Self, CredentialsLock), CredentialsError> {
        let dir = Self::config_dir()?;
        Self::load_locked_in(&dir, account)
    }

    pub(crate) fn load_locked_in(
        dir: &Path,
        account: &str,
    ) -> Result<(Self, CredentialsLock), CredentialsError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| CredentialsError::Io(dir.display().to_string(), e))?;

        let lock_path = Self::lock_path_in(dir, account);
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| CredentialsError::Io(lock_path.display().to_string(), e))?;
        lock_file
            .try_lock_exclusive()
            .map_err(|_e| CredentialsError::Locked {
                account: account.to_string(),
                path: lock_path.clone(),
            })?;
        let lock = CredentialsLock { file: lock_file };

        // 前回 SIGKILL や電源断で取り残された credentials-{account}.toml.* tmp を掃除。
        // lock を取った後に走らせるので他プロセスの書きかけは存在しないと仮定する。
        let prefix = format!("{}.", Self::file_name(account));
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    if !name.starts_with(&prefix) {
                        continue;
                    }
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!("起動時 tmp 掃除に失敗 ({}): {}", path.display(), e);
                    }
                }
            }
            Err(e) => {
                // 掃除は best-effort だが、`read_dir` 自体の失敗 (権限 / FS 障害) は
                // 後段の save / load にも影響するので警告は出す。
                tracing::warn!("起動時 tmp 掃除の read_dir 失敗 ({}): {}", dir.display(), e);
            }
        }

        let path = dir.join(Self::file_name(account));
        let creds = if !path.exists() {
            Self::default()
        } else {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| CredentialsError::Io(path.display().to_string(), e))?;
            toml::from_str(&content)?
        };
        Ok((creds, lock))
    }

    pub fn save(&self, account: &str) -> Result<(), CredentialsError> {
        let dir = Self::config_dir()?;
        self.save_in(&dir, account)
    }

    pub(crate) fn save_in(&self, dir: &Path, account: &str) -> Result<(), CredentialsError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| CredentialsError::Io(dir.display().to_string(), e))?;
        let final_path = dir.join(Self::file_name(account));
        let content = toml::to_string_pretty(self)?;

        let mut tmp = tempfile::Builder::new()
            .prefix(&format!("{}.", Self::file_name(account)))
            .tempfile_in(dir)
            .map_err(|e| CredentialsError::Io(dir.display().to_string(), e))?;
        tmp.as_file_mut()
            .write_all(content.as_bytes())
            .map_err(|e| CredentialsError::Io(tmp.path().display().to_string(), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(tmp.path(), perms)
                .map_err(|e| CredentialsError::Io(tmp.path().display().to_string(), e))?;
        }
        tmp.as_file()
            .sync_all()
            .map_err(|e| CredentialsError::Io(tmp.path().display().to_string(), e))?;
        tmp.persist(&final_path)
            .map_err(|e| CredentialsError::Io(final_path.display().to_string(), e.error))?;

        // 親ディレクトリ fsync (unix のみ、best-effort)。POSIX 仕様上、rename を crash-durable
        // にするには親ディレクトリの fsync が必要だが、tempfile::persist は内部でこれを行わない。
        #[cfg(unix)]
        {
            if let Ok(dir_file) = std::fs::File::open(dir)
                && let Err(e) = dir_file.sync_all()
            {
                tracing::warn!("親ディレクトリ fsync 失敗 ({}): {}", dir.display(), e);
            }
        }

        tracing::debug!("credentials saved to {}", final_path.display());
        Ok(())
    }

    pub fn set_nightbot(&mut self, creds: NightbotCreds) {
        self.nightbot = Some(creds);
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
    #[error("--account {account} は別プロセスが使用中です ({path}). 同じ account を 2 プロセスで起動しないでください")]
    Locked { account: String, path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_nightbot() -> NightbotCreds {
        NightbotCreds {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 1234567890,
            client_id: "cid".to_string(),
            client_secret: "csecret".to_string(),
            callback_port: 8123,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut creds = Credentials::default();
        creds.set_nightbot(sample_nightbot());
        creds.save_in(tmp.path(), "default").unwrap();

        let (loaded, _lock) = Credentials::load_locked_in(tmp.path(), "default").unwrap();
        let nb = loaded.nightbot.unwrap();
        assert_eq!(nb.access_token, "at");
        assert_eq!(nb.refresh_token, "rt");
        assert_eq!(nb.callback_port, 8123);
    }

    #[test]
    fn save_overwrites_existing_file() {
        // persist_noclobber 誤用を CI で検出するため、2 回連続 save しても最後の内容になることを確認
        let tmp = tempfile::tempdir().unwrap();
        let mut creds = Credentials::default();
        let mut first = sample_nightbot();
        first.access_token = "first".to_string();
        creds.set_nightbot(first);
        creds.save_in(tmp.path(), "default").unwrap();

        let mut second = sample_nightbot();
        second.access_token = "second".to_string();
        creds.set_nightbot(second);
        creds.save_in(tmp.path(), "default").unwrap();

        let (loaded, _lock) = Credentials::load_locked_in(tmp.path(), "default").unwrap();
        assert_eq!(loaded.nightbot.unwrap().access_token, "second");
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut creds = Credentials::default();
        creds.set_nightbot(sample_nightbot());
        creds.save_in(tmp.path(), "default").unwrap();

        let path = tmp.path().join("credentials-default.toml");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn load_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (loaded, _lock) = Credentials::load_locked_in(tmp.path(), "default").unwrap();
        assert!(loaded.nightbot.is_none());
    }

    #[test]
    fn load_locked_rejects_second_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        let (_first, _lock1) = Credentials::load_locked_in(tmp.path(), "default").unwrap();
        let err = Credentials::load_locked_in(tmp.path(), "default").unwrap_err();
        assert!(matches!(err, CredentialsError::Locked { .. }));
    }

    #[test]
    fn startup_cleanup_removes_leftover_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        // 残骸 tmp を仕込む
        let leftover = tmp.path().join("credentials-default.toml.dead");
        std::fs::write(&leftover, b"junk").unwrap();
        assert!(leftover.exists());

        let (_, _lock) = Credentials::load_locked_in(tmp.path(), "default").unwrap();
        assert!(!leftover.exists(), "tmp 残骸が掃除されていない");
    }
}
