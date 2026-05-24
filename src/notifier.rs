use crate::config::Config;
use crate::credentials::Credentials;
use crate::sse;
use crate::{twitch, youtube};
use futures::StreamExt;
use tracing::{info, warn};

/// 通知ループ。Ctrl+C や SSE エラーで終了するまで動作する。
pub async fn run(config: Config, mut credentials: Credentials) -> Result<(), NotifierError> {
    let mut stream = sse::connect(&config.detector.sse_url)
        .map_err(|e| NotifierError::Sse(e.to_string()))?;

    info!("通知ループ開始: SSE={}", config.detector.sse_url);

    // YouTube の liveChatId キャッシュ。配信開始のタイミングで取得する
    let mut youtube_live_chat_id: Option<String> = None;

    while let Some(update) = stream.next().await {
        // フィルタ
        if config.filter.auto_only && update.source != "auto" {
            tracing::debug!("skip: source={}", update.source);
            continue;
        }
        let Some(outcome) = update.last_outcome.as_deref() else {
            continue;
        };
        if !["victory", "defeat", "draw"].contains(&outcome) {
            continue;
        }

        let localized = localize_outcome(outcome, &config.messages.language);
        let template = match outcome {
            "victory" => &config.messages.victory,
            "defeat" => &config.messages.defeat,
            "draw" => &config.messages.draw,
            _ => unreachable!(),
        };
        let text = template.replace("{outcome}", localized);
        info!("通知: outcome={} text={}", outcome, text);

        let mut changed = false;

        if config.platforms.twitch_enabled {
            if let Some(creds) = credentials.twitch.as_mut() {
                let before_token = creds.access_token.clone();
                match twitch::send_message(creds, &config.twitch.channel, &text).await {
                    Ok(()) => info!("Twitch 投稿成功"),
                    Err(e) => warn!("Twitch 投稿失敗: {}", e),
                }
                if creds.access_token != before_token {
                    changed = true;
                }
            } else {
                warn!("Twitch が有効ですが認証情報がありません。`auth twitch` を実行してください");
            }
        }

        if config.platforms.youtube_enabled {
            if let Some(creds) = credentials.youtube.as_mut() {
                let before_token = creds.access_token.clone();
                // liveChatId をキャッシュ。未取得/取得失敗時は都度試す
                if youtube_live_chat_id.is_none() {
                    match youtube::get_active_live_chat_id(creds).await {
                        Ok(Some(id)) => {
                            info!("YouTube liveChatId 取得: {}", id);
                            youtube_live_chat_id = Some(id);
                        }
                        Ok(None) => warn!("YouTube 配信が見つかりません (active な配信が無い)"),
                        Err(e) => warn!("YouTube liveChatId 取得失敗: {}", e),
                    }
                }
                if let Some(id) = youtube_live_chat_id.as_deref() {
                    match youtube::send_message(creds, id, &text).await {
                        Ok(()) => info!("YouTube 投稿成功"),
                        Err(e) => {
                            warn!("YouTube 投稿失敗: {}", e);
                            // 配信終了などで liveChatId が無効になった可能性 → キャッシュをクリア
                            youtube_live_chat_id = None;
                        }
                    }
                }
                if creds.access_token != before_token {
                    changed = true;
                }
            } else {
                warn!("YouTube が有効ですが認証情報がありません。`auth youtube` を実行してください");
            }
        }

        if changed {
            if let Err(e) = credentials.save("default") {
                warn!("credentials の保存に失敗: {}", e);
            } else {
                tracing::debug!("credentials を更新保存しました");
            }
        }
    }

    warn!("SSE ストリームが終了しました");
    Ok(())
}

fn localize_outcome(outcome: &str, language: &str) -> &'static str {
    match (outcome, language) {
        ("victory", "ja") => "勝利",
        ("defeat", "ja") => "敗北",
        ("draw", "ja") => "引き分け",
        ("victory", _) => "Victory",
        ("defeat", _) => "Defeat",
        ("draw", _) => "Draw",
        _ => "",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NotifierError {
    #[error("SSE 接続失敗: {0}")]
    Sse(String),
}
