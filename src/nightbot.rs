use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::credentials::NightbotCreds;

const TOKEN_URL: &str = "https://api.nightbot.tv/oauth2/token";
const AUTHORIZE_URL: &str = "https://api.nightbot.tv/oauth2/authorize";

const CALLBACK_DEADLINE: Duration = Duration::from_secs(300);
const RECV_POLL: Duration = Duration::from_millis(250);

const REQUIRED_SCOPES: &[&str] = &["channel", "channel_send"];

#[derive(Debug, thiserror::Error)]
pub enum NightbotError {
    #[error("認証が拒否されました: error={error} description={description:?}")]
    AuthDenied {
        error: String,
        description: Option<String>,
    },
    #[error("OAuth コールバックがタイムアウトしました。`auth nightbot` を再実行してください")]
    CallbackTimeout,
    #[error("OAuth コールバック処理スレッドが落ちました: {0}")]
    CallbackTaskJoin(#[from] tokio::task::JoinError),
    #[error("ローカル HTTP サーバ起動失敗: {0}")]
    ServerStart(String),
    #[error("Nightbot OAuth アプリの許可スコープが不足しています (granted={granted}, missing={missing:?}). アプリ設定を見直して再認証してください")]
    InsufficientScope {
        granted: String,
        missing: Vec<String>,
    },
    #[allow(dead_code)] // 次コミットで send_message から使う
    #[error("メッセージが 400 文字を超えています (len={len})")]
    MessageTooLong { len: usize },
    #[allow(dead_code)] // 次コミットで send_message から使う
    #[error("Nightbot API のレート制限に連続でヒットしました")]
    RateLimited,
    #[error("Nightbot API HTTP {status} error={error:?} desc={description:?} retry_after={retry_after_secs:?} body={raw_body}")]
    HttpStatus {
        status: u16,
        error: Option<String>,
        description: Option<String>,
        raw_body: String,
        retry_after_secs: Option<u64>,
    },
    #[error("Nightbot API レスポンスの JSON パース失敗 (body={body}): {source}")]
    ResponseParse {
        body: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP リクエスト失敗: {0}")]
    Reqwest(#[from] reqwest::Error),
}

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// token endpoint (POST /oauth2/token) のレスポンス本文を `ResponseParse` / `HttpStatus`
/// に詰める前に通すヘルパ。`access_token` / `refresh_token` の値を `REDACTED` に置換する。
fn redact_token_body(body: &str) -> String {
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(obj) = v.as_object_mut()
    {
        for key in ["access_token", "refresh_token"] {
            if obj.contains_key(key) {
                obj[key] = serde_json::Value::String("REDACTED".to_string());
            }
        }
        return serde_json::to_string(&v).unwrap_or_else(|_| "<redact fallback>".to_string());
    }
    // fallback: JSON でなくても "access_token":"..." 形式を文字列ベースで潰す
    let mut s = body.to_string();
    for key in ["access_token", "refresh_token"] {
        s = redact_key_in_string(&s, key);
    }
    s
}

fn redact_key_in_string(s: &str, key: &str) -> String {
    let needle = format!("\"{key}\":\"");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find(&needle) {
        out.push_str(&rest[..idx]);
        out.push_str(&needle);
        out.push_str("REDACTED");
        rest = &rest[idx + needle.len()..];
        if let Some(end) = rest.find('"') {
            out.push('"');
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize, Default)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

struct CallbackResult {
    code: String,
}

/// Authorization Code Flow による初回認証。
/// callback_port は credentials.toml にも保存され、以後の refresh で再利用される。
pub async fn authenticate(
    client_id: &str,
    client_secret: &str,
    callback_port: u16,
) -> Result<NightbotCreds, NightbotError> {
    // 1. CSRF 対策 state (32 bytes random → 手書き hex)
    let mut state_bytes = [0u8; 32];
    getrandom::getrandom(&mut state_bytes)
        .map_err(|e| NightbotError::ServerStart(format!("getrandom 失敗: {e}")))?;
    let state: String = state_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    // 2. callback 待受サーバを spawn_blocking で起動 (recv_timeout ポーリング + Instant deadline)
    let state_for_server = state.clone();
    let server_handle = tokio::task::spawn_blocking(
        move || -> Result<CallbackResult, NightbotError> {
            run_callback_server(callback_port, &state_for_server)
        },
    );

    // 3. 認可 URL を組み立てて表示
    let encoded_id = utf8_percent_encode(client_id, NON_ALPHANUMERIC).to_string();
    let encoded_state = utf8_percent_encode(&state, NON_ALPHANUMERIC).to_string();
    let authorize_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={encoded_id}&redirect_uri=http%3A%2F%2F127.0.0.1%3A{callback_port}%2Fcallback&scope=channel%20channel_send&state={encoded_state}"
    );
    println!("以下の URL をブラウザーで開いて認証してください:\n{authorize_url}");

    let CallbackResult { code } = server_handle.await??;

    // 4. token endpoint で code をトークンに交換
    let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await?;
    let token = parse_token_response(resp).await?;

    // 5. scope 検証 — Nightbot の token response は付与スコープを返す
    let granted: Vec<&str> = token.scope.split_whitespace().collect();
    let missing: Vec<String> = REQUIRED_SCOPES
        .iter()
        .filter(|s| !granted.iter().any(|g| g == *s))
        .map(|s| (*s).to_string())
        .collect();
    if !missing.is_empty() {
        return Err(NightbotError::InsufficientScope {
            granted: token.scope,
            missing,
        });
    }

    // 6. 認証時の callback_port を credentials に保存
    Ok(NightbotCreds {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now_epoch_secs().saturating_add(token.expires_in),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        callback_port,
    })
}

fn run_callback_server(
    callback_port: u16,
    expected_state: &str,
) -> Result<CallbackResult, NightbotError> {
    let server = tiny_http::Server::http(("127.0.0.1", callback_port)).map_err(|e| {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::AddrInUse
        {
            return NightbotError::ServerStart(format!(
                "ポート {callback_port} が既に使用されています。他プロセス終了か `config.toml` の `[nightbot] callback_port` 変更で対処してください (変更時は Nightbot OAuth アプリ側 Redirect URI も合わせる)。確認: `ss -ltnp 'sport = :{callback_port}'`"
            ));
        }
        NightbotError::ServerStart(format!("tiny_http サーバ起動失敗: {e}"))
    })?;

    let deadline = Instant::now() + CALLBACK_DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(NightbotError::CallbackTimeout);
        }
        let wait = RECV_POLL.min(remaining);
        let req = match server.recv_timeout(wait) {
            Ok(Some(req)) => req,
            Ok(None) => continue,
            Err(e) => {
                return Err(NightbotError::ServerStart(format!(
                    "recv_timeout 失敗: {e}"
                )));
            }
        };
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        if path != "/callback" {
            // /favicon.ico などの誤受信は 404 で返して継続。
            let _ = req
                .respond(tiny_http::Response::from_string("not found").with_status_code(404));
            continue;
        }

        let params = parse_query(query);
        let received_state = params.get("state").cloned();
        let state_matches = received_state.as_deref() == Some(expected_state);
        let has_code = params.contains_key("code");
        let has_error = params.contains_key("error");
        info!(
            "callback received: has_code={} has_error={} state_matches={}",
            has_code, has_error, state_matches
        );

        if has_error {
            if !state_matches {
                warn!(
                    "ignoring callback with state {}: has_error",
                    if received_state.is_none() {
                        "missing"
                    } else {
                        "mismatch"
                    }
                );
                let _ = req
                    .respond(tiny_http::Response::from_string("ignored").with_status_code(400));
                continue;
            }
            let error_kind = params.get("error").cloned().unwrap_or_default();
            let description = params.get("error_description").cloned();
            error!(
                "OAuth callback error: error={} description={:?}",
                error_kind, description
            );
            let _ = req.respond(make_html_response(
                "<html><body>認証が拒否されました。詳細は CLI のログを参照してください。</body></html>",
                200,
            ));
            return Err(NightbotError::AuthDenied {
                error: error_kind,
                description,
            });
        }

        if has_code {
            if !state_matches {
                warn!(
                    "ignoring callback with state {}: has_code",
                    if received_state.is_none() {
                        "missing"
                    } else {
                        "mismatch"
                    }
                );
                let _ = req
                    .respond(tiny_http::Response::from_string("ignored").with_status_code(400));
                continue;
            }
            let code = params.get("code").cloned().unwrap_or_default();
            let _ = req.respond(make_html_response(
                "<html><body>認証が完了しました。このウィンドウを閉じてください。</body></html>",
                200,
            ));
            return Ok(CallbackResult { code });
        }

        warn!("ignoring malformed /callback (no code/error)");
        let _ = req.respond(tiny_http::Response::from_string("ignored").with_status_code(400));
    }
}

fn make_html_response(body: &str, status: u16) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/html; charset=utf-8"[..],
            )
            .expect("static header bytes are valid"),
        )
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if query.is_empty() {
        return out;
    }
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        // application/x-www-form-urlencoded: + → 空白 を先に行い、その後 percent-decode する。
        // 逆順だと %2B (リテラル +) まで空白に化けてしまう。
        let k_plus = k.replace('+', " ");
        let v_plus = v.replace('+', " ");
        let k_dec = percent_decode_str(&k_plus).decode_utf8_lossy().into_owned();
        let v_dec = percent_decode_str(&v_plus).decode_utf8_lossy().into_owned();
        out.insert(k_dec, v_dec);
    }
    out
}

async fn parse_token_response(resp: reqwest::Response) -> Result<TokenResponse, NightbotError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await?;
    let redacted = redact_token_body(&body);
    if status.is_success() {
        serde_json::from_str::<TokenResponse>(&body).map_err(|source| {
            NightbotError::ResponseParse {
                body: redacted,
                source,
            }
        })
    } else {
        let err: ErrorBody = serde_json::from_str(&body).unwrap_or_default();
        Err(NightbotError::HttpStatus {
            status: status.as_u16(),
            error: err.error,
            description: err.error_description.or(err.message),
            raw_body: redacted,
            retry_after_secs: retry_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_basic() {
        let q = parse_query("code=abc&state=xyz");
        assert_eq!(q.get("code").map(|s| s.as_str()), Some("abc"));
        assert_eq!(q.get("state").map(|s| s.as_str()), Some("xyz"));
    }

    #[test]
    fn parse_query_plus_then_percent_decode_order() {
        // %2B はリテラル + を表す。+ が空白に化けても %2B は + のまま残るべき。
        let q = parse_query("error_description=user+denied%20now+%2Bplus");
        assert_eq!(
            q.get("error_description").map(|s| s.as_str()),
            Some("user denied now +plus")
        );
    }

    #[test]
    fn parse_query_empty() {
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn parse_query_drops_keys_without_equals() {
        let q = parse_query("foo&bar=1");
        assert_eq!(q.get("bar").map(|s| s.as_str()), Some("1"));
        assert!(!q.contains_key("foo"));
    }

    #[test]
    fn redact_token_body_replaces_tokens_in_json() {
        let body = r#"{"access_token":"secret-at","refresh_token":"secret-rt","expires_in":3600,"scope":"channel channel_send"}"#;
        let red = redact_token_body(body);
        assert!(!red.contains("secret-at"));
        assert!(!red.contains("secret-rt"));
        assert!(red.contains("REDACTED"));
        assert!(red.contains("3600"));
    }

    #[test]
    fn redact_token_body_handles_non_json_fallback() {
        let body = r#"oops "access_token":"leaked" trailing"#;
        let red = redact_token_body(body);
        assert!(!red.contains("leaked"));
        assert!(red.contains("REDACTED"));
    }
}
