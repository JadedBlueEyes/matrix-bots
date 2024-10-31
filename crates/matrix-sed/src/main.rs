mod handlers;

use std::path::Path;

use clap::Parser;
use handlers::on_room_message;
use matrix_sdk::{
    config::SyncSettings,
    matrix_auth::MatrixSession,
    ruma::api::client::{
        filter::FilterDefinition,
        uiaa::{AuthData, Password, UserIdentifier},
    },
    Client, LoopCtrl,
};
use rand::{distributions::Alphanumeric, Rng};
use rpassword::prompt_password;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{error, info, trace, warn};
use tracing_log::AsTrace;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
pub struct Config {
    #[clap(flatten)]
    pub account_config: AccountConfig,

    #[clap(flatten)]
    pub(crate) verbose: clap_verbosity_flag::Verbosity,
}

#[derive(Parser, Debug)]
pub struct AccountConfig {
    /// URL of the homeserver to connect to
    #[arg(short, long, env = "MATRIX_SERVER")]
    pub server: String,
    /// Username of the bot
    #[arg(short, long, env = "MATRIX_USERNAME")]
    pub username: String,
    /// Password of the bot
    #[arg(short, long, env = "MATRIX_PASSWORD")]
    pub password: Option<String>,
    /// Delete devices other than the one being used by this instance
    #[arg(long)]
    pub delete_other_devices: bool,
}

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    /// The URL of the homeserver of the user.
    homeserver: String,

    /// The path of the database.
    db_path: std::path::PathBuf,

    /// The passphrase of the database.
    passphrase: String,
}

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The Matrix user session.
    user_session: MatrixSession,

    /// The latest sync token.
    ///
    /// It is only needed to persist it when using `Client::sync_once()` and we
    /// want to make our syncs faster by not receiving all the initial sync
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Read args
    let config = Config::parse();

    // Logging
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(config.verbose.log_level_filter().as_trace().into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting up");

    let data_dir = dirs::data_dir()
        .expect("no data_dir directory found")
        .join("matrix-sed");
    let session_file = data_dir.join("session");

    let (client, sync_token) = if session_file.exists() {
        restore_session(&session_file).await?
    } else {
        (
            login(&data_dir, &session_file, &config.account_config).await?,
            None,
        )
    };

    run(client, sync_token, &session_file, config).await?;

    Ok(())
}

/// Restore a previous session.
async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, Option<String>)> {
    info!(
        "Previous session found in '{}'",
        session_file.to_string_lossy()
    );

    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized_session)?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .build()
        .await?;

    info!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    Ok((client, sync_token))
}

/// Login to a new session.
async fn login(
    data_dir: &std::path::Path,
    session_file: &std::path::Path,
    config: &AccountConfig,
) -> anyhow::Result<Client> {
    info!("No previous session found, logging in…");
    let mut rng = rand::thread_rng();

    // Generate a random passphrase.
    let passphrase: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    let db_subfolder: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let db_path = data_dir.join(db_subfolder);

    let client = Client::builder()
        .homeserver_url(&config.server)
        .sqlite_store(&db_path, Some(&passphrase))
        .build()
        .await?;

    let client_session = ClientSession {
        homeserver: config.server.clone(),
        db_path,
        passphrase,
    };
    let matrix_auth = client.matrix_auth();

    loop {
        let username = &config.username;
        let password = config.password.clone().unwrap_or_else(|| {
            println!("Type password for the bot (characters won't show up as you type them)");
            match prompt_password("Password: ") {
                Ok(p) => p,
                Err(err) => {
                    panic!("FATAL: failed to get password: {err}");
                }
            }
        });

        match matrix_auth
            .login_username(username, &password)
            .initial_device_display_name("matrix-sed client")
            .await
        {
            Ok(_) => {
                info!("Logged in as {username}");
                break;
            }
            Err(error) => {
                error!("Error logging in: {error}");
                if config.password.is_some() {
                    break;
                }
            }
        }
    }

    // Persist the session to reuse it later.
    // This is not very secure, for simplicity. If the system provides a way of
    // storing secrets securely, it should be used instead.
    // Note that we could also build the user session from the login response.
    let user_session = matrix_auth
        .session()
        .expect("A logged-in client should have a session");
    let serialized_session = serde_json::to_string(&FullSession {
        client_session,
        user_session,
        sync_token: None,
    })?;
    fs::write(session_file, serialized_session).await?;

    info!("Session persisted in {}", session_file.to_string_lossy());

    Ok(client)
}

async fn run(
    client: Client,
    initial_sync_token: Option<String>,
    session_file: &Path,
    config: Config,
) -> anyhow::Result<()> {
    // handler for autojoin
    // Handers here run for historic messages too
    client.add_event_handler(crate::handlers::on_stripped_state_member);

    info!("Launching a first sync to ignore past messages…");

    // Enable room members lazy-loading, it will speed up the initial sync a lot
    // with accounts in lots of rooms.
    // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
    let filter = FilterDefinition::with_lazy_loading();

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    // We restore the sync where we left.
    // This is not necessary when not using `sync_once`. The other sync methods get
    // the sync token from the store.
    if let Some(sync_token) = initial_sync_token {
        sync_settings = sync_settings.token(sync_token);
    }

    // Let's ignore messages before the program was launched.
    // This is a loop in case the initial sync is longer than our timeout. The
    // server should cache the response and it will ultimately take less time to
    // receive.
    loop {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // This is the last time we need to provide this token, the sync method after
                // will handle it on its own.
                sync_settings = sync_settings.token(response.next_batch.clone());
                persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => {
                warn!("An error occurred during initial sync: {error}");
            }
        }
    }
    info!("Initial sync done");

    if config.account_config.delete_other_devices {
        let current_session = client.device_id().map(|d| d.to_owned());
        info!(
            current_session = format!("{current_session:?}"),
            "Checking for other devices to delete"
        );
        let other_devices: Vec<_> = client
            .devices()
            .await?
            .devices
            .iter()
            .filter(|device| Some(&device.device_id) != current_session.as_ref())
            .map(|device| device.device_id.clone())
            .collect();
        if !other_devices.is_empty() {
            trace!(
                current_session = format!("{current_session:?}"),
                other_devices = format!("{other_devices:?}"),
                "Deleting other devices"
            );
            client
                .delete_devices(
                    &other_devices,
                    Some(AuthData::Password(Password::new(
                        UserIdentifier::UserIdOrLocalpart(config.account_config.username.clone()),
                        config.account_config.password.clone().unwrap_or_else(|| {
                            println!(
                            "Type password for the bot (characters won't show up as you type them)"
                        );
                            match prompt_password("Password: ") {
                                Ok(p) => p,
                                Err(err) => {
                                    panic!("FATAL: failed to get password: {err}");
                                }
                            }
                        }),
                    ))),
                )
                .await?;
        }
    }

    // Now that we've synced, attach handlers for new messages.
    client.add_event_handler(on_room_message);

    // This loops until we kill the program or an error happens.
    client
        .sync_with_result_callback(sync_settings, |sync_result| async move {
            let response = sync_result?;

            // We persist the token each time to be able to restore our session
            persist_sync_token(session_file, response.next_batch)
                .await
                .map_err(|err| matrix_sdk::Error::UnknownError(err.into()))?;

            Ok(LoopCtrl::Continue)
        })
        .await?;

    Ok(())
}

/// Persist the sync token for a future session.
/// Note that this is needed only when using `sync_once`. Other sync methods get
/// the sync token from the store.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}
