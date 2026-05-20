use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::credentials::YoutubeCreds;

// ============================================================
// エラー型
// ============================================================

#[derive(Debug, thiserror::Error)]
pub enum YoutubeError {
    #[error("HTTP リクエスト失敗: {0}")]
    Http(#[from] reqwest::Error),
    #[error("YouTube API エラー ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("Device Code Flow がタイムアウトしました")]
    AuthTimeout,
    #[error("Device Code Flow がユーザーに拒否されました")]
    AuthDenied,
    #[error("レスポンスのパース失敗: {0}")]
    Parse(#[from] serde_json::Error),
}

// ============================================================
// 内部用レスポンス型
// ============================================================

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct LiveBroadcastsResponse {
    #[serde(default)]
    items: Vec<LiveBroadcastItem>,
}

#[derive(Deserialize)]
struct LiveBroadcastItem {
    snippet: LiveBroadcastSnippet,
}

#[derive(Deserialize)]
struct LiveBroadcastSnippet {
    #[serde(rename = "liveChatId")]
    live_chat_id: String,
}

// ============================================================
// ユーティリティ
// ============================================================

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// access_token の有効期限まで 60 秒未満なら true
fn needs_refresh(creds: &YoutubeCreds) -> bool {
    creds.expires_at < now_unix() + 60
}

// ============================================================
// 公開 API
// ============================================================

/// Google OAuth2 Device Code Flow で新規認証
pub async fn authenticate(client_id: &str, client_secret: &str) -> Result<YoutubeCreds, YoutubeError> {
    let client = reqwest::Client::new();

    // Step 1: device code を取得
    let resp = client
        .post("https://oauth2.googleapis.com/device/code")
        .form(&[
            ("client_id", client_id),
            ("scope", "https://www.googleapis.com/auth/youtube"),
        ])
        .send()
        .await?;

    let status = resp.status().as_u16();
    let body = resp.text().await?;

    if status != 200 {
        return Err(YoutubeError::Api { status, body });
    }

    let dc: DeviceCodeResponse = serde_json::from_str(&body)?;

    // Step 2: ユーザーに認証を促す
    println!(
        "以下の URL にアクセスして、コード「{}」を入力してください:\n  {}",
        dc.user_code, dc.verification_url
    );

    // Step 3: polling
    let mut interval = dc.interval;
    let deadline = now_unix() + dc.expires_in;

    loop {
        if now_unix() >= deadline {
            return Err(YoutubeError::AuthTimeout);
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let resp = client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("device_code", dc.device_code.as_str()),
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ),
            ])
            .send()
            .await?;

        let poll_status = resp.status().as_u16();
        let poll_body = resp.text().await?;

        if poll_status == 200 {
            let token: TokenResponse = serde_json::from_str(&poll_body)?;
            let expires_at = now_unix() + token.expires_in;
            return Ok(YoutubeCreds {
                access_token: token.access_token,
                refresh_token: token.refresh_token.unwrap_or_default(),
                expires_at,
                client_id: client_id.to_owned(),
                client_secret: client_secret.to_owned(),
            });
        }

        // エラーレスポンスをパース
        let err_resp: Result<TokenErrorResponse, _> = serde_json::from_str(&poll_body);
        match err_resp {
            Ok(e) => match e.error.as_str() {
                "authorization_pending" => {
                    // 継続待機
                }
                "slow_down" => {
                    interval += 5;
                }
                "expired_token" => {
                    return Err(YoutubeError::AuthTimeout);
                }
                "access_denied" => {
                    return Err(YoutubeError::AuthDenied);
                }
                _ => {
                    return Err(YoutubeError::Api {
                        status: poll_status,
                        body: poll_body,
                    });
                }
            },
            Err(_) => {
                return Err(YoutubeError::Api {
                    status: poll_status,
                    body: poll_body,
                });
            }
        }
    }
}

/// refresh_token で access_token を更新 (expires_at も更新)
pub async fn refresh(creds: &mut YoutubeCreds) -> Result<(), YoutubeError> {
    let client = reqwest::Client::new();

    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("refresh_token", creds.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let status = resp.status().as_u16();
    let body = resp.text().await?;

    if status != 200 {
        return Err(YoutubeError::Api { status, body });
    }

    let token: TokenResponse = serde_json::from_str(&body)?;
    creds.access_token = token.access_token;
    creds.expires_at = now_unix() + token.expires_in;
    // refresh_token は返ってこない場合があるので変更しない

    tracing::debug!("YouTube access_token を更新しました (expires_at={})", creds.expires_at);

    Ok(())
}

/// 現在配信中の liveChatId を取得。配信が無ければ Ok(None)。
/// 必要なら refresh を内部で呼ぶ。
pub async fn get_active_live_chat_id(
    creds: &mut YoutubeCreds,
) -> Result<Option<String>, YoutubeError> {
    if needs_refresh(creds) {
        refresh(creds).await?;
    }

    let client = reqwest::Client::new();

    let resp = client
        .get("https://www.googleapis.com/youtube/v3/liveBroadcasts")
        .query(&[
            ("part", "snippet"),
            ("broadcastStatus", "active"),
            ("mine", "true"),
        ])
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", creds.access_token),
        )
        .send()
        .await?;

    let status = resp.status().as_u16();
    let body = resp.text().await?;

    if status != 200 {
        return Err(YoutubeError::Api { status, body });
    }

    let broadcasts: LiveBroadcastsResponse = serde_json::from_str(&body)?;

    Ok(broadcasts
        .items
        .into_iter()
        .next()
        .map(|item| item.snippet.live_chat_id))
}

/// liveChatMessages.insert で 1 件投稿。必要なら refresh を内部で呼ぶ。
pub async fn send_message(
    creds: &mut YoutubeCreds,
    live_chat_id: &str,
    message: &str,
) -> Result<(), YoutubeError> {
    if needs_refresh(creds) {
        refresh(creds).await?;
    }

    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "snippet": {
            "liveChatId": live_chat_id,
            "type": "textMessageEvent",
            "textMessageDetails": {
                "messageText": message
            }
        }
    });

    let resp = client
        .post("https://www.googleapis.com/youtube/v3/liveChat/messages")
        .query(&[("part", "snippet")])
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", creds.access_token),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status().as_u16();
    if status != 200 {
        let resp_body = resp.text().await?;
        return Err(YoutubeError::Api {
            status,
            body: resp_body,
        });
    }

    Ok(())
}
