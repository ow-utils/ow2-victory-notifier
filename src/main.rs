mod config;
mod credentials;
mod notifier;
mod sse;
mod twitch;
mod youtube;

use clap::{Parser, Subcommand};
use config::Config;
use credentials::Credentials;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 認証 (Device Code Flow)
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    match args.command {
        Commands::Auth { platform } => match platform {
            AuthPlatform::Twitch { client_id } => auth_twitch(&client_id).await?,
            AuthPlatform::Youtube { client_id, client_secret } => {
                auth_youtube(&client_id, &client_secret).await?
            }
        },
        Commands::Run { config } => cmd_run(&config).await?,
        Commands::Check { config } => cmd_check(&config).await?,
    }
    Ok(())
}

async fn auth_twitch(client_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let creds = twitch::authenticate(client_id).await?;
    let mut store = Credentials::load()?;
    store.set_twitch(creds);
    store.save()?;
    println!("Twitch の認証情報を保存しました");
    Ok(())
}

async fn auth_youtube(client_id: &str, client_secret: &str) -> Result<(), Box<dyn std::error::Error>> {
    let creds = youtube::authenticate(client_id, client_secret).await?;
    let mut store = Credentials::load()?;
    store.set_youtube(creds);
    store.save()?;
    println!("YouTube の認証情報を保存しました");
    Ok(())
}

async fn cmd_run(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let credentials = Credentials::load()?;
    notifier::run(config, credentials).await?;
    Ok(())
}

async fn cmd_check(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(config_path)?;
    let mut credentials = Credentials::load()?;
    println!("config: ok ({})", config_path);

    // SSE 疎通: 1イベント受信 or 3秒タイムアウト
    println!("SSE 接続テスト: {}", config.detector.sse_url);
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        async {
            use futures::StreamExt;
            let mut s = sse::connect(&config.detector.sse_url).map_err(|e| e.to_string())?;
            let _ = s.next().await;
            Ok::<(), String>(())
        },
    )
    .await
    {
        Ok(Ok(())) => println!("  SSE: 接続成功 (1イベント or タイムアウト前にデータ受信)"),
        Ok(Err(e)) => println!("  SSE: 接続失敗: {}", e),
        Err(_) => println!("  SSE: タイムアウト (接続は成立した可能性あり)"),
    }

    // Twitch
    if config.platforms.twitch_enabled {
        match credentials.twitch.as_mut() {
            Some(c) => match twitch::refresh(c).await {
                Ok(()) => {
                    credentials.save().ok();
                    println!("  Twitch: refresh OK");
                }
                Err(e) => println!("  Twitch: refresh 失敗: {}", e),
            },
            None => println!("  Twitch: 認証情報なし (auth twitch を実行)"),
        }
    }

    // YouTube
    if config.platforms.youtube_enabled {
        match credentials.youtube.as_mut() {
            Some(c) => match youtube::refresh(c).await {
                Ok(()) => {
                    credentials.save().ok();
                    println!("  YouTube: refresh OK");
                }
                Err(e) => println!("  YouTube: refresh 失敗: {}", e),
            },
            None => println!("  YouTube: 認証情報なし (auth youtube を実行)"),
        }
    }

    Ok(())
}
