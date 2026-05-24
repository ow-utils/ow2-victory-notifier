use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::credentials::NightbotCreds;

const TOKEN_URL: &str = "https://api.nightbot.tv/oauth2/token";
const AUTHORIZE_URL: &str = "https://api.nightbot.tv/oauth2/authorize";
const CHANNEL_URL: &str = "https://api.nightbot.tv/1/channel";
const CHANNEL_SEND_URL: &str = "https://api.nightbot.tv/1/channel/send";

const CALLBACK_DEADLINE: Duration = Duration::from_secs(300);
const RECV_POLL: Duration = Duration::from_millis(250);

const REQUIRED_SCOPES: &[&str] = &["channel", "channel_send"];

/// HTTP リクエスト全体のタイムアウト。Nightbot 側が応答を返さないとき SSE ループ全体が
/// 無期限ブロックされて以降の通知が止まるのを防ぐ。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP/TLS 接続確立までのタイムアウト。
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// プロセス共有の reqwest クライアント。毎回 new するとコネクションプール / TLS セットアップを
/// 取り直すコストが乗るため、タイムアウト付きの共有 instance を使う。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .expect("reqwest クライアントの初期化に失敗")
    })
}

/// Nightbot 仕様: /1/channel/send は 400 文字までを受け付ける。
const MESSAGE_MAX_CHARS: usize = 400;
/// access_token 残り時間が これ以下になったら refresh する (秒)。
const REFRESH_MARGIN_SECS: u64 = 60;
/// 429 リトライ時のフォールバック待機 (Nightbot は 5 秒に 1 リクエスト)。
const RATE_LIMIT_FALLBACK_SECS: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum NightbotError {
    #[error("認証が拒否されました: error={error} description={description:?}")]
    AuthDenied {
        error: String,
        description: Option<String>,
    },
    #[error("OAuth コールバックがタイムアウトしました。`auth nightbot` を再実行してください")]
    CallbackTimeout,
    #[error("OAuth コールバック待機中に Ctrl+C で中断されました")]
    CallbackAborted,
    #[error("OAuth コールバック処理スレッドが落ちました: {0}")]
    CallbackTaskJoin(#[from] tokio::task::JoinError),
    #[error("ローカル HTTP サーバ起動失敗: {0}")]
    ServerStart(String),
    #[error("Nightbot OAuth アプリの許可スコープが不足しています (granted={granted}, missing={missing:?}). アプリ設定を見直して再認証してください")]
    InsufficientScope {
        granted: String,
        missing: Vec<String>,
    },
    #[error("メッセージが 400 文字を超えています (len={len})")]
    MessageTooLong { len: usize },
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

impl NightbotError {
    /// 再認証なしには復旧できないエラーかどうか。`refresh` の `invalid_grant`
    /// (refresh_token 失効) や `invalid_client` (OAuth アプリの secret 変更) など、
    /// 再試行で勝手に直らないものを true として呼び出し側にループ終了を促す。
    pub fn is_terminal(&self) -> bool {
        match self {
            NightbotError::HttpStatus { error: Some(e), .. } => {
                matches!(
                    e.as_str(),
                    "invalid_grant"
                        | "invalid_client"
                        | "unauthorized_client"
                        // send_message / get_channel での access_token 失効
                        // (Nightbot 側で revoke されたケース)。再試行で復旧しないため終了。
                        | "invalid_token"
                )
            }
            NightbotError::InsufficientScope { .. } => true,
            _ => false,
        }
    }
}

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 機密キーを `REDACTED` に置換するキーリスト。レスポンス本文を `ResponseParse` /
/// `HttpStatus` に詰める前に必ず通す (Nightbot は通常応答に含めないが、エラー時に
/// `error_description` や echo 形式で混入するケースを防御する)。
const REDACT_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "code",
    "client_secret",
    "id_token",
];

/// レスポンス本文の機密フィールドを潰すヘルパ。常時 redact してから error/parse 系の
/// エラーバリアントに詰める。
fn redact_token_body(body: &str) -> String {
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(body) {
        redact_value(&mut v);
        return serde_json::to_string(&v).unwrap_or_else(|_| "<redact fallback>".to_string());
    }
    // fallback: JSON でなくても "key":"..." / "key": "..." 形式を文字列ベースで潰す
    let mut s = body.to_string();
    for key in REDACT_KEYS {
        s = redact_key_in_string(&s, key);
    }
    s
}

fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(obj) => {
            for (k, val) in obj.iter_mut() {
                if REDACT_KEYS.contains(&k.as_str()) {
                    *val = serde_json::Value::String("REDACTED".to_string());
                } else {
                    redact_value(val);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                redact_value(item);
            }
        }
        _ => {}
    }
}

/// `"key"<spaces>:<spaces>"value"` 形式の value を `REDACTED` に置換する。
/// JSON パース失敗時のフォールバック専用。`"key":"..."` と `"key": "..."` の両方を吸う。
fn redact_key_in_string(s: &str, key: &str) -> String {
    let opener = format!("\"{key}\"");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find(&opener) {
        out.push_str(&rest[..idx]);
        out.push_str(&opener);
        let after_key = &rest[idx + opener.len()..];
        let after_ws = after_key.trim_start();
        let ws_len = after_key.len() - after_ws.len();
        if let Some(after_colon) = after_ws.strip_prefix(':') {
            let value_start = after_colon.trim_start();
            let colon_ws = after_colon.len() - value_start.len();
            if let Some(after_quote) = value_start.strip_prefix('"')
                && let Some(end) = after_quote.find('"')
            {
                out.push_str(&after_key[..ws_len]);
                out.push(':');
                out.push_str(&after_colon[..colon_ws]);
                out.push('"');
                out.push_str("REDACTED");
                out.push('"');
                rest = &after_quote[end + 1..];
                continue;
            }
        }
        // パターンに合わなければそのまま流す (false positive を避ける)
        rest = after_key;
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
    // Option<T> でも serde は missing field をエラー扱いするため、#[serde(default)] が必要。
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

    // 2. callback 待受サーバを spawn_blocking で起動 (recv_timeout ポーリング + Instant deadline)。
    //    Ctrl+C を受けたら abort flag を立て、ループが次の poll tick で即終了する。
    let abort = Arc::new(AtomicBool::new(false));
    let abort_for_signal = abort.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            abort_for_signal.store(true, Ordering::SeqCst);
        }
    });
    let state_for_server = state.clone();
    let abort_for_server = abort.clone();
    let server_handle = tokio::task::spawn_blocking(
        move || -> Result<CallbackResult, NightbotError> {
            run_callback_server(callback_port, &state_for_server, abort_for_server)
        },
    );

    // 3. 認可 URL を組み立てて表示
    let encoded_id = utf8_percent_encode(client_id, NON_ALPHANUMERIC).to_string();
    let encoded_state = utf8_percent_encode(&state, NON_ALPHANUMERIC).to_string();
    let authorize_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={encoded_id}&redirect_uri=http%3A%2F%2F127.0.0.1%3A{callback_port}%2Fcallback&scope=channel%20channel_send&state={encoded_state}"
    );
    println!("以下の URL をブラウザーで開いて認証してください:\n{authorize_url}");
    println!("(Ctrl+C で中断できます)");

    let join_result = server_handle.await?;
    signal_task.abort();
    let CallbackResult { code } = join_result?;

    // 4. token endpoint で code をトークンに交換
    let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");
    let resp = http_client()
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

/// 期限切れまで `REFRESH_MARGIN_SECS` 以下なら refresh を発火する。
/// 戻り値 = refresh が走ったか (true なら呼び出し側は直ちに `credentials.save()` すること)。
pub async fn ensure_fresh_token(creds: &mut NightbotCreds) -> Result<bool, NightbotError> {
    let now = now_epoch_secs();
    // 加算側比較で u64 underflow を回避 (`expires_at - now < margin` だと expires_at < now で wrap)。
    if creds.expires_at > now.saturating_add(REFRESH_MARGIN_SECS) {
        return Ok(false);
    }
    refresh(creds).await?;
    Ok(true)
}

/// refresh_token を使って access_token を更新する。Nightbot は rotation するため
/// 新 refresh_token も同時に保存する。redirect_uri は authorize 時の値と一致必須なため
/// `creds.callback_port` (authenticate 時に保存した値) を使う。
pub async fn refresh(creds: &mut NightbotCreds) -> Result<(), NightbotError> {
    let redirect_uri = format!("http://127.0.0.1:{}/callback", creds.callback_port);
    let req = http_client().post(TOKEN_URL).form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", creds.refresh_token.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", creds.client_id.as_str()),
        ("client_secret", creds.client_secret.as_str()),
    ]);
    let resp = req.send().await?;
    let token = parse_token_response(resp).await?;
    creds.access_token = token.access_token;
    creds.refresh_token = token.refresh_token;
    creds.expires_at = now_epoch_secs().saturating_add(token.expires_in);
    // client_id / client_secret / callback_port は変更しない。
    Ok(())
}

/// 取得済み Bearer トークンで現在 join 中の channel 情報を返す。
pub async fn get_channel(creds: &NightbotCreds) -> Result<ChannelInfo, NightbotError> {
    let req = http_client().get(CHANNEL_URL).bearer_auth(&creds.access_token);
    let resp: ChannelResponse = fetch_json(req).await?;
    Ok(ChannelInfo {
        joined: resp.channel.joined,
        provider: resp.channel.provider,
        name: resp.channel.name,
    })
}

/// channel に投稿する。事前に `ensure_fresh_token` で access_token が新鮮であることを保証すること。
/// 400 文字超過は `MessageTooLong` で送信せず返す (誤った文面が流れるのを避ける)。
/// 429 は `Retry-After` (または 5 秒) 待機して 1 回だけリトライ。
pub async fn send_message(creds: &NightbotCreds, message: &str) -> Result<(), NightbotError> {
    let char_count = message.chars().count();
    if char_count > MESSAGE_MAX_CHARS {
        return Err(NightbotError::MessageTooLong { len: char_count });
    }
    let attempt = || async {
        let req = http_client()
            .post(CHANNEL_SEND_URL)
            .bearer_auth(&creds.access_token)
            .json(&serde_json::json!({ "message": message }));
        fetch_no_body(req).await
    };
    match attempt().await {
        Ok(()) => Ok(()),
        Err(NightbotError::HttpStatus {
            status: 429,
            retry_after_secs,
            ..
        }) => {
            let wait = retry_after_secs.unwrap_or(RATE_LIMIT_FALLBACK_SECS);
            warn!("Nightbot 429: {} 秒待機して 1 回だけリトライします", wait);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match attempt().await {
                Ok(()) => Ok(()),
                Err(NightbotError::HttpStatus { status: 429, .. }) => Err(NightbotError::RateLimited),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub joined: bool,
    pub provider: String,
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct ChannelResponse {
    channel: ChannelObject,
}

#[derive(Deserialize)]
struct ChannelObject {
    joined: bool,
    provider: String,
    #[serde(default)]
    name: Option<String>,
}

/// 共通 HTTP 処理 (成功時は JSON body を T にパース)。エラー本文は常時 redact してから
/// `HttpStatus` / `ResponseParse` に詰める (Nightbot 仕様変更で token がエコーされるリスクを
/// defense-in-depth で潰す)。
async fn fetch_json<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
) -> Result<T, NightbotError> {
    let resp = req.send().await?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await?;
    let recorded = redact_token_body(&body);
    if status.is_success() {
        serde_json::from_str::<T>(&body).map_err(|source| NightbotError::ResponseParse {
            body: recorded,
            source,
        })
    } else {
        let err: ErrorBody = serde_json::from_str(&body).unwrap_or_default();
        Err(NightbotError::HttpStatus {
            status: status.as_u16(),
            error: err.error,
            description: err.error_description.or(err.message),
            raw_body: recorded,
            retry_after_secs: retry_after,
        })
    }
}

/// 成功 body を捨てる版。/1/channel/send で使う (Nightbot は ack を返すだけ)。
/// エラー本文は常時 redact。
async fn fetch_no_body(req: reqwest::RequestBuilder) -> Result<(), NightbotError> {
    let resp = req.send().await?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await?;
    if status.is_success() {
        Ok(())
    } else {
        let recorded = redact_token_body(&body);
        let err: ErrorBody = serde_json::from_str(&recorded).unwrap_or_default();
        Err(NightbotError::HttpStatus {
            status: status.as_u16(),
            error: err.error,
            description: err.error_description.or(err.message),
            raw_body: recorded,
            retry_after_secs: retry_after,
        })
    }
}

fn run_callback_server(
    callback_port: u16,
    expected_state: &str,
    abort: Arc<AtomicBool>,
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
        if abort.load(Ordering::SeqCst) {
            return Err(NightbotError::CallbackAborted);
        }
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
                "認証が拒否されました。詳細は CLI のログを参照してください。",
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
                "認証が完了しました。このウィンドウを閉じてください。",
                200,
            ));
            return Ok(CallbackResult { code });
        }

        warn!("ignoring malformed /callback (no code/error)");
        let _ = req.respond(tiny_http::Response::from_string("ignored").with_status_code(400));
    }
}

fn make_html_response(
    message: &str,
    status: u16,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    // 固定文言のみ埋め込む (外部入力は CLI 側のログにだけ流すため XSS リスクは無いが、
    // DOCTYPE と lang/charset を付けてモバイル含む各ブラウザで日本語が確実に出るようにする)。
    let body = format!(
        "<!DOCTYPE html><html lang=\"ja\"><head><meta charset=\"utf-8\"><title>ow2-victory-notifier</title></head><body>{message}</body></html>"
    );
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

    #[test]
    fn redact_token_body_handles_spaced_json_fallback() {
        // パースが通らない壊れた JSON でも "key": "value" 形式 (コロン後スペース) を吸う
        let body = r#"junk "refresh_token": "leaked-rt" trailing"#;
        let red = redact_token_body(body);
        assert!(!red.contains("leaked-rt"), "spaced fallback failed: {red}");
        assert!(red.contains("REDACTED"));
    }

    #[test]
    fn redact_token_body_redacts_extra_keys() {
        let body = r#"{"code":"abc","client_secret":"shh","id_token":"jwt","other":"keep"}"#;
        let red = redact_token_body(body);
        assert!(!red.contains("abc"));
        assert!(!red.contains("shh"));
        assert!(!red.contains("jwt"));
        assert!(red.contains("keep"));
    }

    #[test]
    fn is_terminal_classifies_invalid_grant() {
        let e = NightbotError::HttpStatus {
            status: 400,
            error: Some("invalid_grant".to_string()),
            description: None,
            raw_body: String::new(),
            retry_after_secs: None,
        };
        assert!(e.is_terminal());

        let e = NightbotError::HttpStatus {
            status: 500,
            error: Some("server_error".to_string()),
            description: None,
            raw_body: String::new(),
            retry_after_secs: None,
        };
        assert!(!e.is_terminal());

        let e = NightbotError::InsufficientScope {
            granted: "channel".to_string(),
            missing: vec!["channel_send".to_string()],
        };
        assert!(e.is_terminal());

        let e = NightbotError::RateLimited;
        assert!(!e.is_terminal());
    }

    #[test]
    fn redact_token_body_walks_nested_objects() {
        let body =
            r#"{"data":{"access_token":"deep-leak","nested":{"refresh_token":"deeper"}},"ok":1}"#;
        let red = redact_token_body(body);
        assert!(!red.contains("deep-leak"));
        assert!(!red.contains("deeper"));
        assert!(red.contains("REDACTED"));
    }
}
