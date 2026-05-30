use eventsource_client::{Client, ClientBuilder, SSE};
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use std::pin::Pin;

#[derive(Debug, Clone, Deserialize)]
// notifier は通知可否判定に last_outcome / source、メッセージ展開に勝敗数を使う。
// SSE スキーマ契約として全フィールドを受理する (必須キーが消えたら早期にパース失敗で気付きたい)。
#[allow(dead_code)]
pub struct CounterUpdate {
    pub victories: u32,
    pub defeats: u32,
    pub draws: u32,
    #[serde(default)]
    pub last_outcome: Option<String>,
    pub timestamp: f64,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "unknown".to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("SSE クライアント構築失敗: {0}")]
    Build(String),
}

/// SSE エンドポイントに接続して CounterUpdate のストリームを返す。
///
/// 内部で eventsource-client の自動再接続を利用する。
/// data JSON のパース失敗は warn ログを出してスキップする (ストリームは継続)。
pub fn connect(url: &str) -> Result<Pin<Box<dyn Stream<Item = CounterUpdate> + Send>>, SseError> {
    let client = ClientBuilder::for_url(url)
        .map_err(|e| SseError::Build(format!("{:?}", e)))?
        .build();

    let stream = client.stream();

    let mapped = stream.filter_map(|ev| async move {
        match ev {
            Ok(SSE::Event(event)) => match serde_json::from_str::<CounterUpdate>(&event.data) {
                Ok(update) => Some(update),
                Err(e) => {
                    tracing::warn!("SSE JSON パース失敗: {} (data={})", e, event.data);
                    None
                }
            },
            Ok(SSE::Comment(_)) | Ok(SSE::Connected(_)) => None,
            Err(e) => {
                tracing::warn!("SSE エラー: {:?}", e);
                None
            }
        }
    });

    Ok(Box::pin(mapped))
}
