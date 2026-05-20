mod config;
mod credentials;
mod notifier;
mod sse;
mod twitch;
mod youtube;

use clap::{Parser, Subcommand};

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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    match args.command {
        Commands::Auth { platform } => match platform {
            AuthPlatform::Twitch { client_id } => {
                tracing::info!("auth twitch: client_id={}", client_id);
                todo!("T5 で実装");
            }
            AuthPlatform::Youtube { client_id, client_secret } => {
                tracing::info!("auth youtube: client_id={} client_secret=*** ", client_id);
                let _ = client_secret;
                todo!("T6 で実装");
            }
        },
        Commands::Run { config } => {
            tracing::info!("run: config={}", config);
            todo!("T7 で実装");
        }
        Commands::Check { config } => {
            tracing::info!("check: config={}", config);
            todo!("T7 で実装");
        }
    }
}
