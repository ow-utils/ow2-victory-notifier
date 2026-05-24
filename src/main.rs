mod config;
mod credentials;
mod nightbot;
mod notifier;
mod sse;
mod twitch;
mod youtube;

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
        #[arg(short, long, default_value = "config.toml")]
        config: String,
    },
    /// 設定と接続の疎通確認
    Check {
        #[arg(short, long, default_value = "config.toml")]
        config: String,
    },
}

#[derive(Subcommand, Debug)]
enum AuthPlatform {
    Twitch {
        #[arg(long)]
        client_id: String,
    },
    Youtube {
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: String,
    },
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

fn validate_account(name: &str) -> Result<(), String> {
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(())
    } else {
        Err(format!(
            "--account 名が不正です: '{name}' (a-zA-Z0-9_- のみ許容、空文字不可)"
        ))
    }
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
            AuthPlatform::Twitch { client_id } => auth_twitch(&client_id).await?,
            AuthPlatform::Youtube {
                client_id,
                client_secret,
            } => auth_youtube(&client_id, &client_secret).await?,
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
        Commands::Run { config } => cmd_run(&config).await?,
        Commands::Check { config } => cmd_check(&config).await?,
    }
    Ok(())
}

async fn auth_twitch(client_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let creds = twitch::authenticate(client_id).await?;
    let (mut store, _lock) = Credentials::load_locked("default")?;
    store.set_twitch(creds);
    store.save("default")?;
    println!("Twitch の認証情報を保存しました");
    Ok(())
}

async fn auth_youtube(
    client_id: &str,
    client_secret: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let creds = youtube::authenticate(client_id, client_secret).await?;
    let (mut store, _lock) = Credentials::load_locked("default")?;
    store.set_youtube(creds);
    store.save("default")?;
    println!("YouTube の認証情報を保存しました");
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
    let creds = nightbot::authenticate(client_id, client_secret, callback_port).await?;
    let (mut store, _lock) = Credentials::load_locked(account)?;
    store.set_nightbot(creds);
    store.save(account)?;
    println!("Nightbot の認証情報を保存しました (account={account})");
    Ok(())
}

async fn cmd_run(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let (credentials, _lock) = Credentials::load_locked("default")?;
    notifier::run(config, credentials).await?;
    Ok(())
}

async fn cmd_check(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let (mut credentials, _lock) = Credentials::load_locked("default")?;
    println!("config: ok ({})", config_path);

    println!("SSE 接続テスト: {}", config.detector.sse_url);
    match tokio::time::timeout(std::time::Duration::from_secs(3), async {
        use futures::StreamExt;
        let mut s = sse::connect(&config.detector.sse_url).map_err(|e| e.to_string())?;
        let _ = s.next().await;
        Ok::<(), String>(())
    })
    .await
    {
        Ok(Ok(())) => println!("  SSE: 接続成功 (1イベント or タイムアウト前にデータ受信)"),
        Ok(Err(e)) => println!("  SSE: 接続失敗: {}", e),
        Err(_) => println!("  SSE: タイムアウト (接続は成立した可能性あり)"),
    }

    if config.platforms.twitch_enabled {
        match credentials.twitch.as_mut() {
            Some(c) => match twitch::refresh(c).await {
                Ok(()) => {
                    credentials.save("default").ok();
                    println!("  Twitch: refresh OK");
                }
                Err(e) => println!("  Twitch: refresh 失敗: {}", e),
            },
            None => println!("  Twitch: 認証情報なし (auth twitch を実行)"),
        }
    }

    if config.platforms.youtube_enabled {
        match credentials.youtube.as_mut() {
            Some(c) => match youtube::refresh(c).await {
                Ok(()) => {
                    credentials.save("default").ok();
                    println!("  YouTube: refresh OK");
                }
                Err(e) => println!("  YouTube: refresh 失敗: {}", e),
            },
            None => println!("  YouTube: 認証情報なし (auth youtube を実行)"),
        }
    }

    Ok(())
}
