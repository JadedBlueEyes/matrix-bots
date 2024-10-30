mod handlers;

use clap::Parser;
use handlers::on_room_message;
use matrix_sdk::{config::SyncSettings, Client};
use rpassword::prompt_password;
use tracing::info;
use tracing_log::AsTrace;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
pub struct Config {
    /// URL of the homeserver to connect to
    #[arg(short, long, env = "MATRIX_SERVER")]
    pub server: String,
    /// Username of the bot
    #[arg(short, long, env = "MATRIX_USERNAME")]
    pub username: String,
    /// Password of the bot
    #[arg(short, long, env = "MATRIX_PASSWORD")]
    pub password: Option<String>,
    #[clap(flatten)]
    pub(crate) verbose: clap_verbosity_flag::Verbosity,
}

async fn login_and_run(config: Config) -> anyhow::Result<()> {
    let client = Client::builder()
        .homeserver_url(config.server)
        .build()
        .await?;

    client
        .matrix_auth()
        .login_username(config.username, &config.password.unwrap())
        .await?;

    // handler for autojoin
    client.add_event_handler(crate::handlers::on_stripped_state_member);

    // initial sync
    let sync_token = client
        .sync_once(SyncSettings::default())
        .await
        .unwrap()
        .next_batch;

    client.add_event_handler(on_room_message);

    let settings = SyncSettings::default().token(sync_token);
    client.sync(settings).await?;

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Read args
    let mut config = Config::parse();

    // Logging
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(config.verbose.log_level_filter().as_trace().into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting up");

    // get connection password

    config.password = Some(config.password.unwrap_or_else(|| {
        println!("Type password for the bot (characters won't show up as you type them)");
        match prompt_password("Password: ") {
            Ok(p) => p,
            Err(err) => {
                panic!("FATAL: failed to get password: {err}");
            }
        }
    }));

    login_and_run(config).await?;

    Ok(())
}
