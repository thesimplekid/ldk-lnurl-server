//! LNURL-pay HTTP service backed by ldk-server.

pub mod store;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use ldk_server_client::{
    client::LdkServerClient,
    ldk_server_grpc::{
        api::Bolt11ReceiveRequest,
        types::{bolt11_invoice_description, Bolt11InvoiceDescription},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::store::{InvoiceStore, IssuedInvoice};

/// Runtime settings for one LNURL-pay address.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address name exposed at `/.well-known/lnurlp/{username}`.
    pub username: String,
    /// Public base URL used to construct the callback URL.
    pub public_url: Url,
    /// Smallest accepted payment in millisatoshis.
    pub min_sendable_msat: u64,
    /// Largest accepted payment in millisatoshis.
    pub max_sendable_msat: u64,
    /// Text shown to payers.
    pub description: String,
    /// Invoice lifetime in seconds.
    pub invoice_expiry_secs: u32,
}

impl Config {
    /// Validate values that would otherwise produce an invalid LNURL response.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.username.is_empty()
            || !self
                .username
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ConfigError::InvalidUsername);
        }
        if !matches!(self.public_url.scheme(), "http" | "https")
            || self.public_url.cannot_be_a_base()
        {
            return Err(ConfigError::InvalidPublicUrl);
        }
        if self.min_sendable_msat == 0 || self.min_sendable_msat > self.max_sendable_msat {
            return Err(ConfigError::InvalidAmountRange);
        }
        if self.description.is_empty() {
            return Err(ConfigError::EmptyDescription);
        }
        if self.invoice_expiry_secs == 0 {
            return Err(ConfigError::InvalidExpiry);
        }
        Ok(())
    }

    fn metadata(&self) -> String {
        serde_json::to_string(&[["text/plain", self.description.as_str()]])
            .expect("serializing string-only LNURL metadata cannot fail")
    }

    fn callback_url(&self) -> Result<Url, ConfigError> {
        self.public_url
            .join(&format!("api/v1/lnurl/callback/{}", self.username))
            .map_err(|_| ConfigError::InvalidPublicUrl)
    }
}

/// Configuration validation failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Username is empty or contains characters unsafe in a path segment.
    #[error("username must contain only ASCII letters, numbers, '.', '-' or '_'")]
    InvalidUsername,
    /// Public URL cannot be used as an HTTP(S) base URL.
    #[error("public URL must be an absolute HTTP(S) base URL")]
    InvalidPublicUrl,
    /// Payment bounds are empty or reversed.
    #[error("minimum amount must be positive and no greater than the maximum")]
    InvalidAmountRange,
    /// Payer-facing description is empty.
    #[error("description must not be empty")]
    EmptyDescription,
    /// Invoices would expire immediately.
    #[error("invoice expiry must be positive")]
    InvalidExpiry,
}

/// Minimal interface needed from a Lightning node.
#[async_trait]
pub trait InvoiceProvider: Send + Sync + 'static {
    /// Create a fixed-amount invoice using the LNURL metadata hash as its description.
    async fn create_invoice(
        &self,
        amount_msat: u64,
        description_hash: String,
        expiry_secs: u32,
    ) -> Result<CreatedInvoice, String>;
}

/// The fields needed to return and track an invoice created by ldk-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedInvoice {
    /// Encoded BOLT11 invoice.
    pub invoice: String,
    /// BOLT11 payment hash returned by ldk-server.
    pub payment_hash: String,
}

/// Adapter for the official ldk-server Rust client.
#[derive(Clone)]
pub struct LdkInvoiceProvider {
    client: LdkServerClient,
}

impl LdkInvoiceProvider {
    /// Wrap an initialized ldk-server client.
    pub fn new(client: LdkServerClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl InvoiceProvider for LdkInvoiceProvider {
    async fn create_invoice(
        &self,
        amount_msat: u64,
        description_hash: String,
        expiry_secs: u32,
    ) -> Result<CreatedInvoice, String> {
        let response = self
            .client
            .bolt11_receive(Bolt11ReceiveRequest {
                amount_msat: Some(amount_msat),
                description: Some(Bolt11InvoiceDescription {
                    kind: Some(bolt11_invoice_description::Kind::Hash(description_hash)),
                }),
                expiry_secs,
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(CreatedInvoice {
            invoice: response.invoice,
            payment_hash: response.payment_hash,
        })
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    invoices: Arc<dyn InvoiceProvider>,
    store: Arc<dyn InvoiceStore>,
    admin_token: Option<Arc<str>>,
}

/// Build the HTTP router.
pub fn router(
    config: Config,
    invoices: Arc<dyn InvoiceProvider>,
    store: Arc<dyn InvoiceStore>,
    admin_token: Option<Arc<str>>,
) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/.well-known/lnurlp/{username}", get(pay_request))
        .route("/api/v1/lnurl/callback/{username}", get(pay_callback))
        .route("/admin/invoices", get(list_invoices))
        .with_state(AppState {
            config,
            invoices,
            store,
            admin_token,
        })
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PayRequest {
    callback: String,
    max_sendable: u64,
    min_sendable: u64,
    metadata: String,
    tag: &'static str,
}

async fn pay_request(
    State(state): State<AppState>,
    Path(username): Path<String>,
) -> impl IntoResponse {
    if username != state.config.username {
        return StatusCode::NOT_FOUND.into_response();
    }

    match state.config.callback_url() {
        Ok(callback) => Json(PayRequest {
            callback: callback.to_string(),
            max_sendable: state.config.max_sendable_msat,
            min_sendable: state.config.min_sendable_msat,
            metadata: state.config.metadata(),
            tag: "payRequest",
        })
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to create callback URL");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    amount: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CallbackSuccess {
    pr: String,
    routes: [(); 0],
}

#[derive(Debug, Serialize)]
struct CallbackError {
    status: &'static str,
    reason: String,
}

async fn pay_callback(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> impl IntoResponse {
    if username != state.config.username {
        return StatusCode::NOT_FOUND.into_response();
    }

    let Some(amount_msat) = query.amount else {
        return lnurl_error("missing amount");
    };
    if !(state.config.min_sendable_msat..=state.config.max_sendable_msat).contains(&amount_msat) {
        return lnurl_error(format!(
            "amount must be between {} and {} millisatoshis",
            state.config.min_sendable_msat, state.config.max_sendable_msat
        ));
    }

    let description_hash = hex::encode(Sha256::digest(state.config.metadata().as_bytes()));
    match state
        .invoices
        .create_invoice(
            amount_msat,
            description_hash,
            state.config.invoice_expiry_secs,
        )
        .await
    {
        Ok(created) => {
            let created_at = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_secs(),
                Err(error) => {
                    tracing::error!(%error, "system clock is before the UNIX epoch");
                    return lnurl_error("could not record invoice");
                }
            };
            let Some(expires_at) = created_at.checked_add(state.config.invoice_expiry_secs.into())
            else {
                tracing::error!(created_at, "invoice expiry timestamp overflowed");
                return lnurl_error("could not record invoice");
            };
            let pr = created.invoice.clone();
            let record = IssuedInvoice::new(
                created.payment_hash,
                created.invoice,
                amount_msat,
                state.config.username.clone(),
                created_at,
                expires_at,
            );
            let store = Arc::clone(&state.store);
            match tokio::task::spawn_blocking(move || store.insert(&record)).await {
                Ok(Ok(())) => Json(CallbackSuccess { pr, routes: [] }).into_response(),
                Ok(Err(error)) => {
                    tracing::error!(%error, "failed to persist issued invoice");
                    lnurl_error("could not record invoice")
                }
                Err(error) => {
                    tracing::error!(%error, "invoice persistence task failed");
                    lnurl_error("could not record invoice")
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, "ldk-server failed to create an invoice");
            lnurl_error("could not create invoice")
        }
    }
}

#[derive(Debug, Serialize)]
struct InvoiceList {
    invoices: Vec<IssuedInvoice>,
}

async fn list_invoices(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let Some(expected_token) = state.admin_token.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !has_bearer_token(&headers, expected_token) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response();
    }

    let store = Arc::clone(&state.store);
    match tokio::task::spawn_blocking(move || store.list()).await {
        Ok(Ok(invoices)) => Json(InvoiceList { invoices }).into_response(),
        Ok(Err(error)) => {
            tracing::error!(%error, "failed to list issued invoices");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(error) => {
            tracing::error!(%error, "invoice listing task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn has_bearer_token(headers: &HeaderMap, expected_token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected_token)
}

fn lnurl_error(reason: impl Into<String>) -> axum::response::Response {
    Json(CallbackError {
        status: "ERROR",
        reason: reason.into(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{body::Body, http::Request};
    use serde_json::{json, Value};
    use tower::ServiceExt as _;

    use super::*;
    use crate::store::StoreError;

    #[derive(Debug, Default)]
    struct FakeInvoices {
        requests: Mutex<Vec<(u64, String, u32)>>,
        failure: bool,
    }

    #[async_trait]
    impl InvoiceProvider for FakeInvoices {
        async fn create_invoice(
            &self,
            amount_msat: u64,
            description_hash: String,
            expiry_secs: u32,
        ) -> Result<CreatedInvoice, String> {
            if self.failure {
                return Err("offline".to_owned());
            }
            self.requests
                .lock()
                .map_err(|error| error.to_string())?
                .push((amount_msat, description_hash, expiry_secs));
            Ok(CreatedInvoice {
                invoice: "lnbc-test-invoice".to_owned(),
                payment_hash: "ab".repeat(32),
            })
        }
    }

    #[derive(Debug, Default)]
    struct FakeStore {
        invoices: Mutex<Vec<IssuedInvoice>>,
    }

    impl InvoiceStore for FakeStore {
        fn insert(&self, invoice: &IssuedInvoice) -> Result<(), StoreError> {
            self.invoices
                .lock()
                .expect("test mutex should not be poisoned")
                .push(invoice.clone());
            Ok(())
        }

        fn list(&self) -> Result<Vec<IssuedInvoice>, StoreError> {
            Ok(self
                .invoices
                .lock()
                .expect("test mutex should not be poisoned")
                .clone())
        }
    }

    fn app(invoices: Arc<dyn InvoiceProvider>) -> (Router, Arc<FakeStore>) {
        let store = Arc::new(FakeStore::default());
        (router(config(), invoices, store.clone(), None), store)
    }

    fn config() -> Config {
        Config {
            username: "alice".to_owned(),
            public_url: Url::parse("https://pay.example.com/").expect("test URL should be valid"),
            min_sendable_msat: 1_000,
            max_sendable_msat: 100_000,
            description: "Pay Alice".to_owned(),
            invoice_expiry_secs: 600,
        }
    }

    async fn json_response(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should answer");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        (
            status,
            serde_json::from_slice(&bytes).expect("response should be JSON"),
        )
    }

    #[tokio::test]
    async fn discovery_advertises_lnurl_pay_contract() {
        let (app, _) = app(Arc::new(FakeInvoices::default()));
        let (status, body) = json_response(app, "/.well-known/lnurlp/alice").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tag"], "payRequest");
        assert_eq!(
            body["callback"],
            "https://pay.example.com/api/v1/lnurl/callback/alice"
        );
        assert_eq!(body["minSendable"], 1_000);
        assert_eq!(body["maxSendable"], 100_000);
        assert_eq!(body["metadata"], r#"[["text/plain","Pay Alice"]]"#);
    }

    #[tokio::test]
    async fn callback_hashes_metadata_and_records_fixed_invoice() {
        let invoices = Arc::new(FakeInvoices::default());
        let (app, store) = app(invoices.clone());
        let (status, body) = json_response(app, "/api/v1/lnurl/callback/alice?amount=21000").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"pr": "lnbc-test-invoice", "routes": []}));
        let requests = invoices
            .requests
            .lock()
            .expect("test mutex should not be poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, 21_000);
        assert_eq!(requests[0].2, 600);
        assert_eq!(
            requests[0].1,
            hex::encode(Sha256::digest(br#"[["text/plain","Pay Alice"]]"#))
        );
        let stored = store
            .invoices
            .lock()
            .expect("test mutex should not be poisoned");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].payment_hash, "ab".repeat(32));
        assert_eq!(stored[0].amount_msat, 21_000);
        assert_eq!(stored[0].username, "alice");
        assert_eq!(stored[0].expires_at - stored[0].created_at, 600);
    }

    #[tokio::test]
    async fn callback_rejects_missing_or_out_of_range_amounts() {
        let (app, _) = app(Arc::new(FakeInvoices::default()));
        let (_, missing) = json_response(app.clone(), "/api/v1/lnurl/callback/alice").await;
        let (_, too_small) = json_response(app, "/api/v1/lnurl/callback/alice?amount=999").await;

        assert_eq!(missing["status"], "ERROR");
        assert_eq!(too_small["status"], "ERROR");
    }

    #[tokio::test]
    async fn callback_hides_backend_errors() {
        let invoices = FakeInvoices {
            requests: Mutex::default(),
            failure: true,
        };
        let (app, store) = app(Arc::new(invoices));
        let (_, body) = json_response(app, "/api/v1/lnurl/callback/alice?amount=21000").await;

        assert_eq!(
            body,
            json!({"status": "ERROR", "reason": "could not create invoice"})
        );
        assert!(store
            .invoices
            .lock()
            .expect("test mutex should not be poisoned")
            .is_empty());
    }

    #[tokio::test]
    async fn unknown_username_is_not_found() {
        let (app, _) = app(Arc::new(FakeInvoices::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/lnurlp/bob")
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should answer");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_invoice_list_requires_configured_bearer_token() {
        let store = Arc::new(FakeStore::default());
        store
            .insert(&IssuedInvoice::new(
                "cd".repeat(32),
                "lnbc-admin-invoice".to_owned(),
                42_000,
                "alice".to_owned(),
                100,
                700,
            ))
            .expect("test invoice should be stored");
        let invoices = Arc::new(FakeInvoices::default());

        let disabled = router(config(), invoices.clone(), store.clone(), None);
        let disabled_response = disabled
            .oneshot(
                Request::builder()
                    .uri("/admin/invoices")
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should answer");
        assert_eq!(disabled_response.status(), StatusCode::NOT_FOUND);

        let token: Arc<str> = Arc::from("correct-admin-token");
        let enabled = router(config(), invoices, store, Some(token));
        let unauthorized = enabled
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/invoices")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should answer");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let (status, body) = json_response_with_authorization(
            enabled,
            "/admin/invoices",
            "Bearer correct-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["invoices"][0]["payment_hash"], "cd".repeat(32));
        assert_eq!(body["invoices"][0]["amount_msat"], 42_000);
    }

    async fn json_response_with_authorization(
        app: Router,
        uri: &str,
        authorization: &str,
    ) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, authorization)
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should answer");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        (
            status,
            serde_json::from_slice(&bytes).expect("response should be JSON"),
        )
    }
}
