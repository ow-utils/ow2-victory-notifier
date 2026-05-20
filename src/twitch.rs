use crate::credentials::TwitchCreds;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::rustls::pki_types::ServerName;

#[derive(Debug, thiserror::Error)]
pub enum TwitchError {
    #[error("HTTP リクエスト失敗: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Twitch API エラー ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("Device Code Flow がタイムアウトしました")]
    AuthTimeout,
    #[error("Device Code Flow がユーザーに拒否されました")]
    AuthDenied,
    #[error("IRC 接続エラー: {0}")]
    Irc(String),
    #[error("TLS 設定エラー: {0}")]
    Tls(String),
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("レスポンスのパース失敗: {0}")]
    Parse(#[from] serde_json::Error),
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---- Device Code Flow レスポンス ----

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    expires_in: u64,
    interval: u64,
    user_code: String,
    verification_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    message: String,
}

// ---- Helix /users ----

#[derive(Deserialize)]
struct HelixUsersResponse {
    data: Vec<HelixUser>,
}

#[derive(Deserialize)]
struct HelixUser {
    login: String,
}

// ---- 公開 API ----

/// Device Code Flow で新規認証し TwitchCreds を返す。
pub async fn authenticate(client_id: &str) -> Result<TwitchCreds, TwitchError> {
    let client = reqwest::Client::new();

    // Step 1: device code 取得
    let resp = client
        .post("https://id.twitch.tv/oauth2/device")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_id={}&scopes=chat:edit+chat:read",
            client_id
        ))
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(TwitchError::Api { status, body });
    }

    let device_resp: DeviceCodeResponse = resp.json().await?;

    println!(
        "次のURLにアクセスし、コード {} を入力してください: {}",
        device_resp.user_code, device_resp.verification_uri
    );

    // Step 2: polling
    let mut interval_secs = device_resp.interval.max(1);
    let deadline = now_unix() + device_resp.expires_in;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

        if now_unix() >= deadline {
            return Err(TwitchError::AuthTimeout);
        }

        let poll_resp = client
            .post("https://id.twitch.tv/oauth2/token")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
                client_id, device_resp.device_code
            ))
            .send()
            .await?;

        let poll_status = poll_resp.status().as_u16();

        if poll_status == 200 {
            let token: TokenResponse = poll_resp.json().await?;
            return Ok(TwitchCreds {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: now_unix() + token.expires_in,
                client_id: client_id.to_owned(),
                login_name: None,
            });
        }

        // エラーレスポンスの message を確認
        let body = poll_resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<TokenErrorResponse>(&body)
            .map(|e| e.message)
            .unwrap_or_default();

        match message.as_str() {
            "authorization_pending" => {
                // 継続
            }
            "slow_down" => {
                interval_secs += 5;
            }
            "expired_token" => {
                return Err(TwitchError::AuthTimeout);
            }
            _ => {
                return Err(TwitchError::AuthDenied);
            }
        }
    }
}

/// refresh_token を用いて access_token を更新する。
pub async fn refresh(creds: &mut TwitchCreds) -> Result<(), TwitchError> {
    let client = reqwest::Client::new();

    let resp = client
        .post("https://id.twitch.tv/oauth2/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            creds.refresh_token, creds.client_id
        ))
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(TwitchError::Api { status, body });
    }

    let token: TokenResponse = resp.json().await?;
    creds.access_token = token.access_token;
    creds.refresh_token = token.refresh_token;
    creds.expires_at = now_unix() + token.expires_in;

    tracing::debug!("Twitch token refreshed, expires_at={}", creds.expires_at);
    Ok(())
}

/// Helix API で自分の login 名を取得し creds にキャッシュする。
pub async fn get_login_name(creds: &mut TwitchCreds) -> Result<String, TwitchError> {
    let client = reqwest::Client::new();

    let resp = client
        .get("https://api.twitch.tv/helix/users")
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("Client-Id", &creds.client_id)
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(TwitchError::Api { status, body });
    }

    let users: HelixUsersResponse = resp.json().await?;
    let login = users
        .data
        .into_iter()
        .next()
        .map(|u| u.login)
        .ok_or_else(|| TwitchError::Api {
            status: 200,
            body: "data array is empty".to_owned(),
        })?;

    creds.login_name = Some(login.clone());
    tracing::debug!("Twitch login_name={}", login);
    Ok(login)
}

/// IRC over TLS でチャンネルに 1 件投稿する。
pub async fn send_message(
    creds: &mut TwitchCreds,
    channel: &str,
    message: &str,
) -> Result<(), TwitchError> {
    // トークン有効期限チェック
    if creds.expires_at < now_unix() + 60 {
        refresh(creds).await?;
    }

    // login_name 未取得なら取得
    let login = match creds.login_name.clone() {
        Some(n) => n,
        None => get_login_name(creds).await?,
    };

    // TLS 設定
    let mut root_store = RootCertStore::empty();
    root_store.roots = webpki_roots::TLS_SERVER_ROOTS.to_vec();
    let tls_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));

    // TCP 接続
    let stream = tokio::net::TcpStream::connect("irc.chat.twitch.tv:6697").await?;

    // TLS ハンドシェイク
    let server_name = ServerName::try_from("irc.chat.twitch.tv".to_owned())
        .map_err(|e| TwitchError::Tls(format!("{:?}", e)))?;
    let mut tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| TwitchError::Irc(format!("TLS connect: {}", e)))?;

    let channel_lc = channel.trim_start_matches('#').to_lowercase();
    let cmds = format!(
        "PASS oauth:{}\r\nNICK {}\r\nJOIN #{}\r\nPRIVMSG #{} :{}\r\nQUIT\r\n",
        creds.access_token,
        login.to_lowercase(),
        channel_lc,
        channel_lc,
        message
    );

    tls.write_all(cmds.as_bytes()).await?;
    tls.flush().await?;

    // 応答を少し読み捨て
    let mut buf = [0u8; 1024];
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tls.read(&mut buf),
    )
    .await;

    tracing::debug!("IRC message sent to #{}", channel_lc);
    Ok(())
}
