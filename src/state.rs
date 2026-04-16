use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;
use axum::http::StatusCode;

use crate::model::{IncomingApi, WireApi};
use crate::routing::RouterManager;
use crate::session::SessionStore;

#[derive(Clone, Debug)]
pub(crate) struct UpstreamSuccessLogSnapshot {
    pub(crate) router_name: String,
    pub(crate) incoming_api: IncomingApi,
    pub(crate) upstream_wire: WireApi,
    pub(crate) status: StatusCode,
    pub(crate) headers: Value,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) client: Client,
    pub(crate) api_key: String,
    pub(crate) http_shutdown: bool,
    pub(crate) verbose_logging: bool,
    pub(crate) routers: Arc<RwLock<RouterManager>>,
    pub(crate) sessions: Arc<RwLock<SessionStore>>,
    pub(crate) last_successful_upstream_log: Arc<RwLock<Option<UpstreamSuccessLogSnapshot>>>,
}
