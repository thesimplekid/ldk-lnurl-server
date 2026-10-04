use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};

use ldk_lnurl_server::{
    router,
    store::{RedbInvoiceStore, StoreError},
    Config, LdkInvoiceProvider,
};
use ldk_server_client::client::LdkServerClient;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:3000/";
const DEFAULT_LDK_SERVER_ADDRESS: &str = "127.0.0.1:3536";
const DEFAULT_DATABASE_PATH: &str = "lnurl.redb";

#[derive(Debug, Error)]
enum StartupError {
    #[error("invalid {name}: {message}")]
    InvalidSetting { name: &'static str, message: String },
    #[error("failed to read {name} from {path}: {source}")]
    ReadCredential {
        name: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to configure ldk-server client: {0}")]
    LdkClient(String),
    #[error("failed to open invoice database: {0}")]
    Store(#[from] StoreError),
    #[error("failed to bind HTTP server to {address}: {source}")]
    Bind {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("HTTP server failed: {0}")]
    Serve(std::io::Error),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = run().await {
        tracing::error!(%error, "server stopped");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), StartupError> {
    let bind_addr = parse_env("LNURL_BIND_ADDR", DEFAULT_BIND_ADDR)?;
    let config = Config {
        username: env_or("LNURL_USERNAME", "sats"),
        public_url: parse_env("LNURL_PUBLIC_URL", DEFAULT_PUBLIC_URL)?,
        min_sendable_msat: parse_env("LNURL_MIN_SENDABLE_MSAT", "1000")?,
        max_sendable_msat: parse_env("LNURL_MAX_SENDABLE_MSAT", "100000000")?,
        description: env_or("LNURL_DESCRIPTION", "Pay with Lightning"),
        invoice_expiry_secs: parse_env("LNURL_INVOICE_EXPIRY_SECS", "3600")?,
    };
    config
        .validate()
        .map_err(|error| StartupError::InvalidSetting {
            name: "LNURL configuration",
            message: error.to_string(),
        })?;

    let cert_path = required_path("LDK_SERVER_TLS_CERT_FILE")?;
    let macaroon_path = required_path("LDK_SERVER_MACAROON_FILE")?;
    let cert = read_credential("LDK_SERVER_TLS_CERT_FILE", &cert_path)?;
    let macaroon = String::from_utf8(read_credential("LDK_SERVER_MACAROON_FILE", &macaroon_path)?)
        .map_err(|error| StartupError::InvalidSetting {
            name: "LDK_SERVER_MACAROON_FILE",
            message: format!("must contain UTF-8 hex: {error}"),
        })?;
    let client = LdkServerClient::new(
        env_or("LDK_SERVER_ADDRESS", DEFAULT_LDK_SERVER_ADDRESS),
        macaroon.trim().to_owned(),
        &cert,
    )
    .map_err(StartupError::LdkClient)?;
    let database_path = PathBuf::from(env_or("LNURL_DATABASE_PATH", DEFAULT_DATABASE_PATH));
    let store = Arc::new(RedbInvoiceStore::open(&database_path)?);
    let admin_token = optional_admin_token("LNURL_ADMIN_TOKEN_FILE")?;

    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|source| StartupError::Bind {
            address: bind_addr,
            source,
        })?;
    tracing::info!(
        %bind_addr,
        username = %config.username,
        database = %database_path.display(),
        admin_api = admin_token.is_some(),
        "LNURL server listening"
    );
    axum::serve(
        listener,
        router(
            config,
            Arc::new(LdkInvoiceProvider::new(client)),
            store,
            admin_token,
        ),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(StartupError::Serve)
}

fn env_or(name: &'static str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn parse_env<T>(name: &'static str, default: &str) -> Result<T, StartupError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = env_or(name, default);
    value
        .parse::<T>()
        .map_err(|error| StartupError::InvalidSetting {
            name,
            message: error.to_string(),
        })
}

fn required_path(name: &'static str) -> Result<PathBuf, StartupError> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| StartupError::InvalidSetting {
            name,
            message: "environment variable is required".to_owned(),
        })
}

fn read_credential(name: &'static str, path: &PathBuf) -> Result<Vec<u8>, StartupError> {
    std::fs::read(path).map_err(|source| StartupError::ReadCredential {
        name,
        path: path.clone(),
        source,
    })
}

fn optional_admin_token(name: &'static str) -> Result<Option<Arc<str>>, StartupError> {
    let Some(path) = env::var_os(name).map(PathBuf::from) else {
        return Ok(None);
    };
    let bytes = read_credential(name, &path)?;
    let token = String::from_utf8(bytes).map_err(|error| StartupError::InvalidSetting {
        name,
        message: format!("must contain UTF-8 text: {error}"),
    })?;
    let token = token.trim();
    if token.len() < 32 {
        return Err(StartupError::InvalidSetting {
            name,
            message: "token must be at least 32 bytes".to_owned(),
        });
    }
    Ok(Some(Arc::from(token)))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for Ctrl-C");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
