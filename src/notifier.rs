use crate::config::{Config, MessagesConfig};
use crate::credentials::{Credentials, CredentialsError, CredentialsLock, NightbotCreds};
use crate::nightbot::{self, NightbotError};
use crate::sse;
use futures::StreamExt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

/// SSE ストリーム終了時の再接続バックオフ初期値。失敗のたび倍にして上限で頭打ち。
const SSE_RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const SSE_RECONNECT_MAX: Duration = Duration::from_secs(60);
/// 1 セッションがこの時間以上維持できたら「健全な接続だった」とみなしてバックオフをリセット。
/// 短期間に複数回切断 → 60 秒まで肥大 → その後安定稼働、というケースで次回再接続が
/// 不必要に遅延し続けるのを防ぐ。
const SSE_HEALTHY_DURATION: Duration = Duration::from_secs(60);

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
    // テンプレート展開後の文面が上限を超える設定では毎試合投稿が落ち続けるため、
    // SSE ループに入る前に弾く。
    validate_message_lengths(&config.messages)?;

    // Nightbot 一本化後は投稿経路がこれのみ。credentials が無いまま SSE ループに入ると
    // 毎イベント warn を出すだけで永久に投稿できず、しかも account lock を保持し続けるため
    // 同 account の `auth nightbot` も lock 競合で実行できない (サイレントな無機能状態)。
    // validate_message_lengths と同様にループ突入前に弾いて非ゼロ終了させる。
    if credentials.nightbot.is_none() {
        return Err(NotifierError::NoCredentials {
            account: account.to_string(),
        });
    }

    // lock のライフタイムを通知ループ全体に明示的にバインドする。Drop されると
    // flock が解放されて二重起動防止が崩れるため、関数終了まで保持し続ける。
    let _lock = lock;

    // counter 側は SSE 接続直後に必ず現在状態 (直近の last_broadcast) のスナップショットを
    // 送る。これは過去に処理済みの勝敗 (last_outcome / source) を保持し得るため、起動・再接続
    // のたびに直前の結果を重複投稿してしまう。各イベントの timestamp で dedupe し、起動時刻
    // 以前 (= 監視開始前に発生した結果) と再接続時の再送 (= 既処理の最大 timestamp 以下) を弾く。
    // 起動時刻を初期下限にすることで、cold start 時のスナップショット投稿も同時に防ぐ。
    let mut last_processed_ts = unix_secs_f64();
    let mut backoff = SSE_RECONNECT_INITIAL;
    loop {
        let started = Instant::now();
        match run_once(&config, &mut credentials, account, &mut last_processed_ts).await {
            Ok(()) => {
                let lifetime = started.elapsed();
                if lifetime >= SSE_HEALTHY_DURATION {
                    info!(
                        "SSE セッションが {} 秒維持できたためバックオフをリセットします",
                        lifetime.as_secs()
                    );
                    backoff = SSE_RECONNECT_INITIAL;
                }
                warn!(
                    "SSE ストリームが終了しました。{} 秒後に再接続します",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(SSE_RECONNECT_MAX);
            }
            Err(NotifierError::Sse(e)) => {
                warn!(
                    "SSE 接続失敗 ({}). {} 秒後に再試行します",
                    e,
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(SSE_RECONNECT_MAX);
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
    last_processed_ts: &mut f64,
) -> Result<(), NotifierError> {
    let mut stream =
        sse::connect(&config.detector.sse_url).map_err(|e| NotifierError::Sse(e.to_string()))?;
    info!(
        "通知ループ開始: SSE={} account={}",
        config.detector.sse_url, account
    );

    while let Some(update) = stream.next().await {
        // 接続直後のスナップショットや再接続時の再送など、既に処理済み (= 最大 timestamp 以下)
        // のイベントを無視する。新しい結果は必ず新しい timestamp を持つため取りこぼさない。
        if is_replayed(update.timestamp, *last_processed_ts) {
            tracing::debug!(
                "skip replayed/stale event: ts={} <= {}",
                update.timestamp,
                last_processed_ts
            );
            continue;
        }
        *last_processed_ts = update.timestamp;

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
            Err(e) if e.is_terminal() => {
                error!(
                    "token refresh が再試行不能なエラー ({e})。`auth nightbot --account {account}` で再認証してください"
                );
                return Err(NotifierError::TerminalAuth {
                    account: account.to_string(),
                    source: e,
                });
            }
            Err(e) => {
                warn!("token refresh 失敗 (継続): {}", e);
                continue;
            }
        }

        let creds_ref: &NightbotCreds = credentials
            .nightbot
            .as_ref()
            .expect("nightbot creds were Some above and refresh does not unset");
        match nightbot::send_message(creds_ref, &text).await {
            Ok(()) => info!("Nightbot 投稿成功"),
            Err(e) if e.is_terminal() => {
                error!(
                    "Nightbot 投稿が再試行不能なエラー ({e})。`auth nightbot --account {account}` で再認証してください"
                );
                return Err(NotifierError::TerminalAuth {
                    account: account.to_string(),
                    source: e,
                });
            }
            Err(e) => warn!("Nightbot 投稿失敗 (継続): {}", e),
        }
    }

    Ok(())
}

/// 現在の UNIX 時刻を秒 (f64) で返す。SSE イベントの timestamp (counter 側が
/// `as_secs_f64` で生成) と同じ尺度で dedupe するために使う。
fn unix_secs_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// イベントが既処理 (接続直後のスナップショット再送や再接続時の重複) かを判定する。
/// counter 側のスナップショットは直近 broadcast と同一 timestamp を持つため、これまでに
/// 処理した最大 timestamp 以下なら再送とみなす。起動時刻を初期下限にすることで、監視開始
/// 前に発生した結果 (cold start のスナップショット) も同時に弾く。
fn is_replayed(event_ts: f64, last_processed_ts: f64) -> bool {
    event_ts <= last_processed_ts
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
    #[error("Nightbot 認証が再認証必須 (account={account}): {source}. `auth nightbot --account {account}` を実行してください")]
    TerminalAuth {
        account: String,
        #[source]
        source: NightbotError,
    },
    #[error("Nightbot 認証情報がありません (account={account})。`auth nightbot --account {account}` を実行してください")]
    NoCredentials { account: String },
    #[error("messages.{outcome} の文面がテンプレート展開後 {len} 文字で Nightbot の上限 {max} 文字を超えています。config.toml を見直してください")]
    MessageTooLong {
        outcome: String,
        len: usize,
        max: usize,
    },
}

/// config のテンプレートを展開した文面が Nightbot の文字数上限を超えないか検証する。
/// run / check の起動時に呼び、毎試合 `MessageTooLong` で投稿が落ち続ける設定を早期に弾く。
pub fn validate_message_lengths(messages: &MessagesConfig) -> Result<(), NotifierError> {
    for outcome in ["victory", "defeat", "draw"] {
        let len = build_message(messages, outcome).chars().count();
        if len > nightbot::MESSAGE_MAX_CHARS {
            return Err(NotifierError::MessageTooLong {
                outcome: outcome.to_string(),
                len,
                max: nightbot::MESSAGE_MAX_CHARS,
            });
        }
    }
    Ok(())
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

    #[test]
    fn is_replayed_skips_snapshot_and_reconnect_replay() {
        let start = 1000.0;
        // cold start: 監視開始前 (start 以下) のスナップショットは弾く。
        assert!(is_replayed(999.0, start));
        assert!(is_replayed(1000.0, start));
        // 開始後に発生した新しい結果は処理する。
        assert!(!is_replayed(1001.0, start));
        // 処理済み (last=1001) の結果が再接続スナップショットで再送されても弾く。
        assert!(is_replayed(1001.0, 1001.0));
        // さらに新しい結果は処理する。
        assert!(!is_replayed(1002.0, 1001.0));
    }

    #[test]
    fn validate_message_lengths_accepts_normal() {
        assert!(validate_message_lengths(&msgs("ja")).is_ok());
    }

    #[test]
    fn validate_message_lengths_rejects_overlong() {
        let mut m = msgs("ja");
        m.defeat = "あ".repeat(nightbot::MESSAGE_MAX_CHARS + 1);
        match validate_message_lengths(&m) {
            Err(NotifierError::MessageTooLong { outcome, len, max }) => {
                assert_eq!(outcome, "defeat");
                assert_eq!(len, nightbot::MESSAGE_MAX_CHARS + 1);
                assert_eq!(max, nightbot::MESSAGE_MAX_CHARS);
            }
            other => panic!("expected MessageTooLong, got {other:?}"),
        }
    }

    #[test]
    fn validate_message_lengths_counts_chars_after_expansion() {
        // {outcome} 展開後の文字数で判定する (テンプレート自体は短くても展開で超えうる)。
        let mut m = msgs("ja");
        m.victory = format!("{{outcome}}{}", "x".repeat(nightbot::MESSAGE_MAX_CHARS));
        assert!(validate_message_lengths(&m).is_err());
    }
}
