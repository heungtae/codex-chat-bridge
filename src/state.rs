use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::routing::RouterManager;
use crate::session::SessionStore;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) client: Client,
    pub(crate) api_key: String,
    pub(crate) http_shutdown: bool,
    pub(crate) verbose_logging: bool,
    pub(crate) routers: Arc<RwLock<RouterManager>>,
    pub(crate) sessions: Arc<RwLock<SessionStore>>,
}
