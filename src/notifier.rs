use crate::config::{Config, MessagesConfig};
use crate::credentials::{Credentials, CredentialsError, CredentialsLock, NightbotCreds};
use crate::nightbot;
use crate::sse;
use futures::StreamExt;
use std::time::Duration;
use tracing::{error, info, warn};

/// SSE ストリーム終了時の再接続バックオフ初期値。失敗のたび倍にして上限で頭打ち。
const SSE_RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const SSE_RECONNECT_MAX: Duration = Duration::from_secs(60);

/// 通知ループ。Nightbot 経由でライブチャットに勝敗を投稿する。
/// eventsource-client は内部再接続を行うが、まれにストリーム自体が終了するため
/// (counter 側の長時間ダウンや TLS セッション枯渇) 外側でもバックオフ付きで再構築する。
/// 致命エラー (CredentialsSaveFailed) のみ Err で抜け、それ以外は無限ループ。
pub async fn run(
    config: Config,
    mut credentials: Credentials,
    lock: CredentialsLock,
    account: &str,
) -> Result<(), NotifierError> {
    // lock のライフタイムを通知ループ全体に明示的にバインドする。Drop されると
    // flock が解放されて二重起動防止が崩れるため、関数終了まで保持し続ける。
    let _lock = lock;
    let mut backoff = SSE_RECONNECT_INITIAL;
    loop {
        match run_once(&config, &mut credentials, account).await {
            Ok(()) => {
                warn!(
                    "SSE ストリームが終了しました。{} 秒後に再接続します",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(SSE_RECONNECT_MAX);
            }
            Err(NotifierError::Sse(e)) => {
                warn!(
                    "SSE 接続失敗 ({}). {} 秒後に再試行します",
                    e,
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(SSE_RECONNECT_MAX);
            }
            Err(e) => return Err(e),
        }
    }
}

/// SSE ストリーム 1 セッション分の処理。終了 (ストリーム完了) または致命エラーで戻る。
async fn run_once(
    config: &Config,
    credentials: &mut Credentials,
    account: &str,
) -> Result<(), NotifierError> {
    let mut stream =
        sse::connect(&config.detector.sse_url).map_err(|e| NotifierError::Sse(e.to_string()))?;
    info!(
        "通知ループ開始: SSE={} account={}",
        config.detector.sse_url, account
    );

    while let Some(update) = stream.next().await {
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

        let text = build_message(&config.messages, outcome);
        info!("通知: outcome={} text={}", outcome, text);

        let Some(creds) = credentials.nightbot.as_mut() else {
            warn!(
                "Nightbot 認証情報がありません ({0}). `auth nightbot --account {0}` を実行してください",
                account
            );
            continue;
        };

        // refresh が走ったら send より前に save。save 失敗時はメモリ上の (新) token と
        // ファイル上の (旧、Nightbot 側で既に失効) token が乖離するため、修復不能状態を作らない
        // よう即終了する。
        match nightbot::ensure_fresh_token(creds).await {
            Ok(true) => {
                if let Err(e) = credentials.save(account) {
                    error!(
                        "rotation 後の credentials 保存に失敗。修復不能の場合は `auth nightbot --account {}` で再認証してください: {}",
                        account, e
                    );
                    return Err(NotifierError::CredentialsSaveFailed {
                        account: account.to_string(),
                        source: e,
                    });
                }
            }
            Ok(false) => {}
            Err(e) => {
                warn!("token refresh 失敗: {}", e);
                continue;
            }
        }

        let creds_ref: &NightbotCreds = credentials
            .nightbot
            .as_ref()
            .expect("nightbot creds were Some above and refresh does not unset");
        match nightbot::send_message(creds_ref, &text).await {
            Ok(()) => info!("Nightbot 投稿成功"),
            Err(e) => warn!("Nightbot 投稿失敗: {}", e),
        }
    }

    Ok(())
}

fn build_message(messages: &MessagesConfig, outcome: &str) -> String {
    let localized = localize_outcome(outcome, &messages.language);
    let template = match outcome {
        "victory" => &messages.victory,
        "defeat" => &messages.defeat,
        "draw" => &messages.draw,
        _ => return String::new(),
    };
    template.replace("{outcome}", localized)
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
    #[error("credentials 保存失敗 (account={account}): {source}")]
    CredentialsSaveFailed {
        account: String,
        #[source]
        source: CredentialsError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(language: &str) -> MessagesConfig {
        MessagesConfig {
            language: language.to_string(),
            victory: "{outcome}！".to_string(),
            defeat: "{outcome}…".to_string(),
            draw: "{outcome}".to_string(),
        }
    }

    #[test]
    fn build_message_ja() {
        let m = msgs("ja");
        assert_eq!(build_message(&m, "victory"), "勝利！");
        assert_eq!(build_message(&m, "defeat"), "敗北…");
        assert_eq!(build_message(&m, "draw"), "引き分け");
    }

    #[test]
    fn build_message_en() {
        let m = msgs("en");
        assert_eq!(build_message(&m, "victory"), "Victory！");
        assert_eq!(build_message(&m, "defeat"), "Defeat…");
        assert_eq!(build_message(&m, "draw"), "Draw");
    }

    #[test]
    fn build_message_template_without_placeholder() {
        let mut m = msgs("ja");
        m.victory = "固定文言".to_string();
        assert_eq!(build_message(&m, "victory"), "固定文言");
    }

    #[test]
    fn build_message_unknown_outcome_returns_empty() {
        let m = msgs("ja");
        assert_eq!(build_message(&m, "unknown"), "");
    }
}
