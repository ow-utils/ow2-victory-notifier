mod config;
mod credentials;
mod nightbot;
mod notifier;
mod sse;

use clap::{Args, Parser, Subcommand};
use config::Config;
use credentials::Credentials;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct CliArgs {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 認証
    Auth {
        #[command(subcommand)]
        platform: AuthPlatform,
    },
    /// 通知ループを実行
    Run {
        #[arg(short, long, default_value = "default")]
        account: String,
        #[arg(short, long, default_value = "config.toml")]
        config: String,
    },
    /// 設定と接続の疎通確認
    Check {
        #[arg(short, long, default_value = "default")]
        account: String,
        #[arg(short, long, default_value = "config.toml")]
        config: String,
        /// 期限に関係なく refresh_token を rotate して延命する
        /// (run 常駐中は lock 競合で失敗する点に注意)。
        #[arg(long)]
        force_refresh: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuthPlatform {
    /// Nightbot OAuth (Authorization Code Flow)。
    /// callback_port は config から取得し、credentials に保存する。
    Nightbot {
        /// アカウント名 (a-zA-Z0-9_- のみ)。credentials-{account}.toml を読み書きする。
        #[arg(short, long, default_value = "default")]
        account: String,
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        client_id: String,
        #[command(flatten)]
        secret: NightbotClientSecret,
    },
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
struct NightbotClientSecret {
    /// client_secret 直渡し (シェル履歴・ps に残るため非推奨)。
    #[arg(long)]
    client_secret: Option<String>,
    /// client_secret を読み取る環境変数名 (推奨)。
    #[arg(long, value_name = "ENV_VAR_NAME")]
    client_secret_env: Option<String>,
}

impl NightbotClientSecret {
    fn resolve(&self) -> Result<String, Box<dyn std::error::Error>> {
        if let Some(s) = &self.client_secret {
            return Ok(s.clone());
        }
        if let Some(var) = &self.client_secret_env {
            return Ok(std::env::var(var)
                .map_err(|e| format!("環境変数 {var} の読み取りに失敗: {e}"))?);
        }
        Err("--client-secret または --client-secret-env のどちらかを指定してください".into())
    }
}

/// account 名はファイル名 (`credentials-{account}.toml`) になるため、ファイルシステム上限
/// (ext4 で 255 byte) より十分手前で弾く。長さ無制限だとパス由来の I/O エラーになり
/// バリデーション NG だと気付きにくい。
const ACCOUNT_MAX_LEN: usize = 64;

fn validate_account(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("--account 名が空です".to_string());
    }
    if name.len() > ACCOUNT_MAX_LEN {
        return Err(format!(
            "--account 名が長すぎます ({} > {ACCOUNT_MAX_LEN})",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "--account 名が不正です: '{name}' (a-zA-Z0-9_- のみ許容)"
        ));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = CliArgs::parse();
    match args.command {
        Commands::Auth { platform } => match platform {
            AuthPlatform::Nightbot {
                account,
                config,
                client_id,
                secret,
            } => {
                validate_account(&account)?;
                let client_secret = secret.resolve()?;
                auth_nightbot(&account, &config, &client_id, &client_secret).await?;
            }
        },
        Commands::Run { account, config } => {
            validate_account(&account)?;
            cmd_run(&account, &config).await?;
        }
        Commands::Check {
            account,
            config,
            force_refresh,
        } => {
            validate_account(&account)?;
            cmd_check(&account, &config, force_refresh).await?;
        }
    }
    Ok(())
}

async fn auth_nightbot(
    account: &str,
    config_path: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let callback_port = config.nightbot.callback_port;
    // OAuth フローの前に lock を取る。authenticate() は最大 5 分のブラウザ承認 + token
    // 交換を行うため、これを完了させてから lock 競合で弾くとユーザの承認操作と発行済み
    // authorization code (使い捨て) が無駄になる。同一 account の二重 auth も検出可能。
    let (mut store, _lock) = Credentials::load_locked(account)?;
    let creds = nightbot::authenticate(client_id, client_secret, callback_port).await?;
    store.set_nightbot(creds);
    store.save(account)?;
    println!("Nightbot の認証情報を保存しました (account={account})");
    Ok(())
}

async fn cmd_run(account: &str, config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let (credentials, lock) = Credentials::load_locked(account)?;
    notifier::run(config, credentials, lock, account).await?;
    Ok(())
}

async fn cmd_check(
    account: &str,
    config_path: &str,
    force_refresh: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let (mut credentials, _lock) = Credentials::load_locked(account)?;
    println!("config: ok ({})", config_path);
    println!("account: {} (callback_port={})", account, config.nightbot.callback_port);

    println!("SSE 接続テスト: {}", config.detector.sse_url);
    // 低速回線・proxy 経由だと TLS ハンドシェイク + 初回 event 受信に時間がかかるため、
    // 3 秒だと「timeout だけど実は成立」のミスリードが出やすい。10 秒に伸ばす。
    match tokio::time::timeout(std::time::Duration::from_secs(10), async {
        use futures::StreamExt;
        let mut s = sse::connect(&config.detector.sse_url).map_err(|e| e.to_string())?;
        let _ = s.next().await;
        Ok::<(), String>(())
    })
    .await
    {
        Ok(Ok(())) => println!("  SSE: 接続成功"),
        Ok(Err(e)) => println!("  SSE: 接続失敗: {}", e),
        Err(_) => println!("  SSE: タイムアウト (接続は成立した可能性あり)"),
    }

    let Some(creds) = credentials.nightbot.as_mut() else {
        println!(
            "  Nightbot: 認証情報なし (`auth nightbot --account {account}` を実行)"
        );
        return Ok(());
    };

    // expires_at から残時間を表示
    let now = nightbot::now_epoch_secs();
    let remaining_secs = creds.expires_at.saturating_sub(now);
    println!(
        "  access_token: 残り {} 分 ({} 秒)",
        remaining_secs / 60,
        remaining_secs
    );

    // refresh: --force-refresh なら無条件、それ以外は ensure_fresh_token に委ねる。
    // 直接 nightbot::refresh を呼んでから ensure_fresh_token に渡すと、後者が Ok(false) を
    // 返して save が走らなくなり新 refresh_token を失う。--force-refresh 経路でも refresh →
    // 即 save の順を守る。
    let refreshed = if force_refresh {
        match nightbot::refresh(creds).await {
            Ok(()) => {
                println!("  Nightbot: --force-refresh で refresh OK");
                true
            }
            Err(e) => {
                println!("  Nightbot: refresh 失敗: {}", e);
                return Ok(());
            }
        }
    } else {
        match nightbot::ensure_fresh_token(creds).await {
            Ok(true) => {
                println!("  Nightbot: 期限間近につき refresh OK");
                true
            }
            Ok(false) => {
                println!("  Nightbot: 期限十分のため refresh スキップ");
                false
            }
            Err(e) => {
                println!("  Nightbot: refresh 失敗: {}", e);
                return Ok(());
            }
        }
    };
    if refreshed && let Err(e) = credentials.save(account) {
        // notifier::run と同じ方針で「rotation 後の save 失敗」は致命扱いにする。
        // Ok(()) で抜けるとユーザは延命に成功したと誤認しがちなので必ず非ゼロ終了させる。
        return Err(format!(
            "Nightbot: credentials 保存失敗 ({e})。新 refresh_token がディスクに永続化されていません。`auth nightbot --account {account}` で再認証してください"
        )
        .into());
    }

    // 必要なら credentials を再借用 (save の所有権)
    let Some(creds) = credentials.nightbot.as_ref() else {
        return Ok(());
    };

    match nightbot::get_channel(creds).await {
        Ok(info) => {
            println!(
                "  Nightbot channel: provider={} name={:?} joined={}",
                info.provider, info.name, info.joined
            );
            if !info.joined {
                tracing::warn!(
                    "Nightbot が channel に Join していません。https://nightbot.tv/ ダッシュボードで Join 操作をしてください (YouTube は配信 live + public 時のみ自動 join)"
                );
            }
        }
        Err(e) => println!("  Nightbot get_channel 失敗: {}", e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_account_accepts_valid_names() {
        assert!(validate_account("default").is_ok());
        assert!(validate_account("twitch_1").is_ok());
        assert!(validate_account("a-b-c").is_ok());
        assert!(validate_account("A").is_ok());
    }

    #[test]
    fn validate_account_rejects_empty() {
        assert!(validate_account("").is_err());
    }

    #[test]
    fn validate_account_rejects_path_traversal() {
        assert!(validate_account("../etc").is_err());
        assert!(validate_account("a/b").is_err());
        assert!(validate_account(".").is_err());
    }

    #[test]
    fn validate_account_rejects_non_ascii() {
        assert!(validate_account("日本語").is_err());
        assert!(validate_account("a b").is_err());
    }

    #[test]
    fn validate_account_rejects_overlong() {
        let name = "a".repeat(ACCOUNT_MAX_LEN + 1);
        assert!(validate_account(&name).is_err());
        let name = "a".repeat(ACCOUNT_MAX_LEN);
        assert!(validate_account(&name).is_ok());
    }
}
