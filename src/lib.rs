use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::HOST;
use axum::http::uri::Authority;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use clap::Parser;
use clap::ValueEnum;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fs::File;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;
use tracing::info;
use uuid::Uuid;

mod bridge;
use bridge::mapping::*;
use bridge::streaming::*;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum WireApi {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingApi {
    Responses,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpstreamHeader {
    name: String,
    value: String,
}

const DEFAULT_FORWARDED_UPSTREAM_HEADERS: [&str; 5] = [
    "openai-organization",
    "openai-project",
    "x-openai-subagent",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
];

#[derive(Debug, Clone, Parser)]
#[command(
    name = "codex-chat-bridge",
    about = "Responses-to-Chat completions bridge",
    version
)]
struct Args {
    #[arg(
        long,
        value_name = "FILE",
        help = "config file path (default: ~/.config/codex-chat-bridge/conf.toml)"
    )]
    config: Option<PathBuf>,

    #[arg(long)]
    upstream_url: Option<String>,

    #[arg(long, value_enum)]
    upstream_wire: Option<WireApi>,

    #[arg(
        long = "upstream-http-header",
        value_name = "NAME=VALUE",
        action = clap::ArgAction::Append,
        value_parser = parse_upstream_http_header_arg,
        help = "add static header sent to upstream requests; can be repeated"
    )]
    upstream_http_headers: Vec<UpstreamHeader>,

    #[arg(
        long = "forward-incoming-header",
        value_name = "NAME",
        action = clap::ArgAction::Append,
        help = "forward this incoming header onto the upstream request; can be repeated"
    )]
    forward_incoming_headers: Vec<String>,

    #[arg(long)]
    api_key_env: Option<String>,

    #[arg(long, value_name = "FILE")]
    server_info: Option<PathBuf>,

    #[arg(long)]
    http_shutdown: bool,

    #[arg(long, help = "enable verbose bridge logs (request/response payloads)")]
    verbose_logging: bool,

    #[arg(
        long = "drop-tool-type",
        value_name = "TYPE",
        action = clap::ArgAction::Append,
        help = "drop tool entries whose `type` matches this value; can be repeated"
    )]
    drop_tool_types: Vec<String>,

    #[arg(
        long,
        short = 'r',
        alias = "profile",
        help = "use router from config file"
    )]
    router: Option<String>,

    #[arg(
        long,
        alias = "list-profiles",
        help = "list available routers from config file and exit"
    )]
    list_routers: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FileConfig {
    upstream_url: Option<String>,
    upstream_wire: Option<WireApi>,
    #[serde(alias = "http_headers")]
    upstream_http_headers: Option<BTreeMap<String, String>>,
    forward_incoming_headers: Option<Vec<String>>,
    api_key_env: Option<String>,
    server_info: Option<PathBuf>,
    http_shutdown: Option<bool>,
    verbose_logging: Option<bool>,
    drop_tool_types: Option<Vec<String>>,
    drop_request_fields: Option<Vec<String>>,
    features: Option<FeatureFlagsConfig>,
    #[serde(alias = "profiles")]
    routers: Option<BTreeMap<String, RouterConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RouterConfig {
    upstream_url: Option<String>,
    upstream_wire: Option<WireApi>,
    #[serde(alias = "http_headers")]
    upstream_http_headers: Option<BTreeMap<String, String>>,
    forward_incoming_headers: Option<Vec<String>>,
    drop_tool_types: Option<Vec<String>>,
    drop_request_fields: Option<Vec<String>>,
    features: Option<FeatureFlagsConfig>,
    incoming_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FeatureFlagsConfig {
    enable_previous_response_id: Option<bool>,
    enable_tool_argument_stream_events: Option<bool>,
    enable_extended_stream_events: Option<bool>,
    enable_reasoning_stream_events: Option<bool>,
    enable_provider_specific_fields: Option<bool>,
    enable_extended_input_types: Option<bool>,
}

#[derive(Debug, Clone)]
struct ResolvedConfig {
    upstream_url: String,
    upstream_wire: WireApi,
    upstream_http_headers: Vec<UpstreamHeader>,
    forward_incoming_headers: Vec<String>,
    api_key_env: String,
    server_info: Option<PathBuf>,
    http_shutdown: bool,
    verbose_logging: bool,
    drop_tool_types: Vec<String>,
    drop_request_fields: Vec<String>,
    feature_flags: FeatureFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeatureFlags {
    enable_previous_response_id: bool,
    enable_tool_argument_stream_events: bool,
    enable_extended_stream_events: bool,
    enable_reasoning_stream_events: bool,
    enable_provider_specific_fields: bool,
    enable_extended_input_types: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            enable_previous_response_id: true,
            enable_tool_argument_stream_events: true,
            enable_extended_stream_events: true,
            enable_reasoning_stream_events: true,
            enable_provider_specific_fields: true,
            enable_extended_input_types: true,
        }
    }
}

impl FeatureFlags {
    fn with_overrides(mut self, overrides: Option<&FeatureFlagsConfig>) -> Self {
        let Some(overrides) = overrides else {
            return self;
        };
        if let Some(v) = overrides.enable_previous_response_id {
            self.enable_previous_response_id = v;
        }
        if let Some(v) = overrides.enable_tool_argument_stream_events {
            self.enable_tool_argument_stream_events = v;
        }
        if let Some(v) = overrides.enable_extended_stream_events {
            self.enable_extended_stream_events = v;
        }
        if let Some(v) = overrides.enable_reasoning_stream_events {
            self.enable_reasoning_stream_events = v;
        }
        if let Some(v) = overrides.enable_provider_specific_fields {
            self.enable_provider_specific_fields = v;
        }
        if let Some(v) = overrides.enable_extended_input_types {
            self.enable_extended_input_types = v;
        }
        self
    }
}

const DEFAULT_CONFIG_TEMPLATE: &str = r#"# codex-chat-bridge runtime configuration
#
# Priority: CLI flags > config file > built-in defaults

# upstream_url = "https://api.openai.com/v1/chat/completions"
# upstream_wire = "chat" # chat | responses
# upstream_http_headers = { "openai-organization" = "org_123", "x-custom-header" = "value" }
# forward_incoming_headers = ["x-codex-turn-state"]
# api_key_env = "OPENAI_API_KEY"
# server_info = "/tmp/codex-chat-bridge-info.json"
# http_shutdown = false
# verbose_logging = false
# drop_tool_types = ["web_search", "web_search_preview"]
# drop_request_fields = ["prompt_cache_key"]
#
# [features]
# enable_previous_response_id = true
# enable_tool_argument_stream_events = true
# enable_extended_stream_events = true
# enable_reasoning_stream_events = true
# enable_provider_specific_fields = true
# enable_extended_input_types = true

# [routers.default]
# upstream_url = "https://api.openai.com/v1/chat/completions"
# upstream_wire = "chat"
# upstream_http_headers = {}
# forward_incoming_headers = []
# drop_tool_types = []
# drop_request_fields = []
# incoming_url = "http://<host>:<port>/default"
# [routers.default.features]
# enable_reasoning_stream_events = false

# [routers.prod]
# upstream_url = "https://api.openai.com/v1/chat/completions"
# upstream_wire = "responses"
# incoming_url = "http://<host>:<port>/gpt-oss"

# [routers.dev]
# upstream_url = "http://<host>:<port>/v1/chat/completions"
"#;

#[derive(Clone)]
struct AppState {
    client: Client,
    api_key: String,
    http_shutdown: bool,
    verbose_logging: bool,
    routers: Arc<RwLock<RouterManager>>,
    sessions: Arc<RwLock<SessionStore>>,
}

#[derive(Clone)]
struct RouterManager {
    routers: BTreeMap<String, RouterConfig>,
    default_upstream_url: String,
    default_upstream_wire: WireApi,
    default_upstream_http_headers: Vec<UpstreamHeader>,
    default_forward_incoming_headers: Vec<String>,
    default_drop_tool_types: HashSet<String>,
    default_drop_request_fields: HashSet<String>,
    default_feature_flags: FeatureFlags,
    incoming_route_to_router: BTreeMap<IncomingRouteKey, String>,
    listen_addrs: BTreeSet<String>,
}

#[derive(Clone)]
struct RouteTarget {
    router_name: String,
    upstream_url: String,
    upstream_wire: WireApi,
    upstream_http_headers: Vec<UpstreamHeader>,
    forward_incoming_headers: Vec<String>,
    drop_tool_types: HashSet<String>,
    drop_request_fields: HashSet<String>,
    feature_flags: FeatureFlags,
}

#[derive(Clone, Debug)]
struct RouterDefaultsLogSnapshot {
    upstream_url: String,
    upstream_wire: WireApi,
    upstream_http_headers: Vec<UpstreamHeader>,
    forward_incoming_headers: Vec<String>,
    drop_tool_types: Vec<String>,
    drop_request_fields: Vec<String>,
}

#[derive(Clone, Debug)]
struct RouterDeltaLogSnapshot {
    name: String,
    active: bool,
    incoming_url: Option<String>,
    upstream_wire: WireApi,
    override_upstream_url: Option<String>,
    override_upstream_wire: Option<WireApi>,
    override_upstream_http_headers: Option<Vec<UpstreamHeader>>,
    override_forward_incoming_headers: Option<Vec<String>>,
    override_drop_tool_types: Option<Vec<String>>,
    override_drop_request_fields: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IncomingRouteKey {
    authority: Option<String>,
    path: String,
}

#[derive(Debug)]
struct ParsedIncomingUrl {
    route_key: IncomingRouteKey,
    bind_addr: Option<String>,
}

impl RouterManager {
    fn new(
        routers: BTreeMap<String, RouterConfig>,
        default_upstream_url: String,
        default_upstream_wire: WireApi,
        default_upstream_http_headers: Vec<UpstreamHeader>,
        default_forward_incoming_headers: Vec<String>,
        default_drop_tool_types: Vec<String>,
        default_drop_request_fields: Vec<String>,
        default_feature_flags: FeatureFlags,
    ) -> Result<Self> {
        let default_forward_incoming_headers = if default_forward_incoming_headers.is_empty() {
            DEFAULT_FORWARDED_UPSTREAM_HEADERS
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
        } else {
            default_forward_incoming_headers
        };

        let mut incoming_route_to_router = BTreeMap::new();
        let mut listen_addrs = BTreeSet::new();
        for (router_name, router_config) in &routers {
            let incoming_url = router_config.incoming_url.as_ref().ok_or_else(|| {
                anyhow!(
                    "missing required incoming_url for [routers.{router_name}]. default(active-router) routing is disabled"
                )
            })?;
            let effective_router_upstream_url = router_config
                .upstream_url
                .as_deref()
                .unwrap_or(&default_upstream_url);
            resolve_upstream_wire(
                Some(effective_router_upstream_url),
                router_config.upstream_wire,
                default_upstream_wire,
                &format!("[routers.{router_name}]"),
            )?;
            let parsed = parse_incoming_url(incoming_url).with_context(|| {
                format!(
                    "invalid incoming_url for [routers.{router_name}] => {}",
                    incoming_url
                )
            })?;
            if let Some(bind_addr) = parsed.bind_addr {
                listen_addrs.insert(bind_addr);
            }
            let route_key = parsed.route_key;
            if let Some(existing_router) =
                incoming_route_to_router.insert(route_key.clone(), router_name.clone())
            {
                return Err(anyhow!(
                    "duplicated incoming_url route '{}' for routers '{}' and '{}'",
                    describe_route_key(&route_key),
                    existing_router,
                    router_name
                ));
            }
        }

        Ok(Self {
            routers,
            default_upstream_url,
            default_upstream_wire,
            default_upstream_http_headers,
            default_forward_incoming_headers,
            default_drop_tool_types: default_drop_tool_types.into_iter().collect(),
            default_drop_request_fields: default_drop_request_fields.into_iter().collect(),
            default_feature_flags,
            incoming_route_to_router,
            listen_addrs,
        })
    }

    fn get_router_names(&self) -> Vec<String> {
        self.routers.keys().cloned().collect()
    }

    fn get_default_log_snapshot(&self) -> RouterDefaultsLogSnapshot {
        let mut drop_tool_types = self
            .default_drop_tool_types
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        drop_tool_types.sort();
        let mut drop_request_fields = self
            .default_drop_request_fields
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        drop_request_fields.sort();
        RouterDefaultsLogSnapshot {
            upstream_url: self.default_upstream_url.clone(),
            upstream_wire: self.default_upstream_wire,
            upstream_http_headers: self.default_upstream_http_headers.clone(),
            forward_incoming_headers: self.default_forward_incoming_headers.clone(),
            drop_tool_types,
            drop_request_fields,
        }
    }

    fn get_router_delta_log_snapshots(&self) -> Vec<RouterDeltaLogSnapshot> {
        let default_headers = self
            .default_upstream_http_headers
            .iter()
            .map(|h| (h.name.to_ascii_lowercase(), h.value.clone()))
            .collect::<BTreeMap<_, _>>();

        let mut snapshots = Vec::with_capacity(self.routers.len());
        for (name, router_cfg) in &self.routers {
            let override_upstream_url = router_cfg
                .upstream_url
                .clone()
                .filter(|url| *url != self.default_upstream_url);
            let effective_router_upstream_url = router_cfg
                .upstream_url
                .as_deref()
                .unwrap_or(&self.default_upstream_url);
            let resolved_upstream_wire = resolve_upstream_wire(
                Some(effective_router_upstream_url),
                router_cfg.upstream_wire,
                self.default_upstream_wire,
                &format!("[routers.{name}]"),
            )
            .ok()
            .unwrap_or(self.default_upstream_wire);
            let override_upstream_wire = (resolved_upstream_wire != self.default_upstream_wire)
                .then_some(resolved_upstream_wire);

            let override_upstream_http_headers = router_cfg.upstream_http_headers.as_ref().and_then(
                |router_headers| {
                    let mut overrides = Vec::new();
                    for (header_name, header_value) in router_headers {
                        let key = header_name.to_ascii_lowercase();
                        if default_headers.get(&key) == Some(header_value) {
                            continue;
                        }
                        overrides.push(UpstreamHeader {
                            name: header_name.clone(),
                            value: header_value.clone(),
                        });
                    }
                    if overrides.is_empty() {
                        None
                    } else {
                        Some(overrides)
                    }
                },
            );

            let override_forward_incoming_headers = router_cfg
                .forward_incoming_headers
                .clone()
                .filter(|headers| *headers != self.default_forward_incoming_headers);

            let override_drop_tool_types = router_cfg.drop_tool_types.as_ref().and_then(|types| {
                let mut extras = types
                    .iter()
                    .filter(|t| !t.trim().is_empty() && !self.default_drop_tool_types.contains(*t))
                    .cloned()
                    .collect::<Vec<_>>();
                extras.sort();
                extras.dedup();
                if extras.is_empty() {
                    None
                } else {
                    Some(extras)
                }
            });
            let override_drop_request_fields =
                router_cfg.drop_request_fields.as_ref().and_then(|fields| {
                    let mut extras = fields
                        .iter()
                        .map(|f| f.trim())
                        .filter(|f| !f.is_empty() && !self.default_drop_request_fields.contains(*f))
                        .map(ToString::to_string)
                        .collect::<Vec<_>>();
                    extras.sort();
                    extras.dedup();
                    if extras.is_empty() {
                        None
                    } else {
                        Some(extras)
                    }
                });

            snapshots.push(RouterDeltaLogSnapshot {
                name: name.clone(),
                active: true,
                incoming_url: router_cfg.incoming_url.clone(),
                upstream_wire: resolved_upstream_wire,
                override_upstream_url,
                override_upstream_wire,
                override_upstream_http_headers,
                override_forward_incoming_headers,
                override_drop_tool_types,
                override_drop_request_fields,
            });
        }

        snapshots
    }

    fn get_listen_addrs(&self) -> Vec<String> {
        self.listen_addrs.iter().cloned().collect()
    }

    fn get_target_for_incoming_route(
        &self,
        path: &str,
        host_header: Option<&str>,
    ) -> Result<Option<RouteTarget>> {
        let normalized_path = normalize_request_path(path);
        if let Some(authority) =
            host_header.and_then(normalize_host_header_to_authority)
        {
            let key = IncomingRouteKey {
                authority: Some(authority),
                path: normalized_path.clone(),
            };
            if let Some(router_name) = self.incoming_route_to_router.get(&key) {
                return self.resolve_target_for_router_name(router_name).map(Some);
            }
        }

        let key = IncomingRouteKey {
            authority: None,
            path: normalized_path,
        };
        let Some(router_name) = self.incoming_route_to_router.get(&key) else {
            return Ok(None);
        };
        self.resolve_target_for_router_name(router_name).map(Some)
    }

    fn resolve_target_for_router_name(&self, name: &str) -> Result<RouteTarget> {
        let router = if self.routers.is_empty() {
            None
        } else {
            Some(
                self.routers
                    .get(name)
                    .ok_or_else(|| anyhow!("router '{}' not found", name))?,
            )
        };

        let upstream_url = router
            .and_then(|r| r.upstream_url.clone())
            .unwrap_or_else(|| self.default_upstream_url.clone());

        let upstream_wire = resolve_upstream_wire(
            Some(&upstream_url),
            router.and_then(|r| r.upstream_wire),
            self.default_upstream_wire,
            &format!("router '{name}'"),
        )?;

        let mut upstream_http_headers = self.default_upstream_http_headers.clone();
        if let Some(router_headers) = router.and_then(|r| r.upstream_http_headers.clone()) {
            for (name, value) in router_headers {
                let header = UpstreamHeader {
                    name: name.clone(),
                    value,
                };
                upsert_upstream_http_header(&mut upstream_http_headers, header);
            }
        }

        let mut forward_incoming_headers = self.default_forward_incoming_headers.clone();
        if let Some(router_forward) = router.and_then(|r| r.forward_incoming_headers.clone()) {
            forward_incoming_headers.clear();
            for header in router_forward {
                if let Ok(validated) = validate_forward_incoming_header(header.clone()) {
                    forward_incoming_headers.push(validated);
                }
            }
        }

        let mut drop_tool_types = self.default_drop_tool_types.clone();
        if let Some(router_drop) = router.and_then(|r| r.drop_tool_types.clone()) {
            drop_tool_types.extend(router_drop);
        }
        let mut drop_request_fields = self.default_drop_request_fields.clone();
        if let Some(router_drop_fields) = router.and_then(|r| r.drop_request_fields.clone()) {
            for field in router_drop_fields {
                let normalized = field.trim();
                if !normalized.is_empty() {
                    drop_request_fields.insert(normalized.to_string());
                }
            }
        }
        let feature_flags =
            self.default_feature_flags
                .with_overrides(router.and_then(|r| r.features.as_ref()));

        Ok(RouteTarget {
            router_name: name.to_string(),
            upstream_url,
            upstream_wire,
            upstream_http_headers,
            forward_incoming_headers,
            drop_tool_types,
            drop_request_fields,
            feature_flags,
        })
    }
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    ports: Vec<u16>,
    pid: u32,
}

#[derive(Debug)]
struct BridgeRequest {
    chat_request: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesToolCallKind {
    Function,
    Custom,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[allow(dead_code)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Option<ChatDelta>,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    added_emitted: bool,
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    assistant_text: String,
    reasoning_text: String,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    usage: Option<ChatUsage>,
}

#[derive(Debug)]
struct SessionStore {
    messages_by_response_id: HashMap<String, Vec<Value>>,
    insertion_order: VecDeque<String>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            messages_by_response_id: HashMap::new(),
            insertion_order: VecDeque::new(),
        }
    }
}

impl SessionStore {
    const MAX_ENTRIES: usize = 1024;

    fn get_messages(&self, response_id: &str) -> Option<Vec<Value>> {
        self.messages_by_response_id.get(response_id).cloned()
    }

    fn insert_messages(&mut self, response_id: String, messages: Vec<Value>) {
        if self.messages_by_response_id.contains_key(&response_id) {
            self.insertion_order.retain(|id| id != &response_id);
        }
        self.messages_by_response_id
            .insert(response_id.clone(), messages);
        self.insertion_order.push_back(response_id);

        while self.messages_by_response_id.len() > Self::MAX_ENTRIES {
            let Some(oldest_id) = self.insertion_order.pop_front() else {
                break;
            };
            self.messages_by_response_id.remove(&oldest_id);
        }
    }
}

#[derive(Debug, Default)]
struct SseParser {
    buffer: String,
    current_data_lines: Vec<String>,
}

#[derive(Debug)]
struct NormalizedUpstreamError {
    code: String,
    message: String,
}

fn init_tracing(verbose_logging: bool) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose_logging {
            tracing_subscriber::EnvFilter::new("debug")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    let config_path = resolve_config_path(args.config.clone())?;
    ensure_default_config_file(&config_path)?;
    let file_config = load_file_config(&config_path)?;
    let config = resolve_config(args.clone(), file_config.clone())?;
    init_tracing(config.verbose_logging);

    let routers = file_config
        .as_ref()
        .and_then(|fc| fc.routers.clone())
        .unwrap_or_default();

    if args.list_routers {
        if routers.is_empty() {
            println!("No routers defined in config file.");
        } else {
            println!("Available routers:");
            for name in routers.keys() {
                println!("  - {}", name);
            }
        }
        return Ok(());
    }

    let api_key = std::env::var(&config.api_key_env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing or empty env var: {}", config.api_key_env))?;

    let client = Client::builder()
        .build()
        .context("building reqwest client")?;

    let router_manager = RouterManager::new(
        routers,
        config.upstream_url.clone(),
        config.upstream_wire,
        config.upstream_http_headers,
        config.forward_incoming_headers,
        config.drop_tool_types,
        config.drop_request_fields,
        config.feature_flags,
    )?;
    let listen_addrs = router_manager.get_listen_addrs();
    if listen_addrs.is_empty() {
        return Err(anyhow!(
            "no listenable incoming_url found. configure at least one absolute URL like `http://<host>:<port>/<path>` in [routers.*].incoming_url"
        ));
    }
    let router_defaults = router_manager.get_default_log_snapshot();

    info!(
        "startup: listen_addrs={:?} router_count={}",
        listen_addrs,
        router_manager.get_router_names().len()
    );
    info!(
        "router defaults: upstream_url={}, upstream_wire={:?}, upstream_http_headers={:?}, forward_incoming_headers={:?}, drop_tool_types={:?}, drop_request_fields={:?}",
        router_defaults.upstream_url,
        router_defaults.upstream_wire,
        router_defaults.upstream_http_headers,
        router_defaults.forward_incoming_headers,
        router_defaults.drop_tool_types,
        router_defaults.drop_request_fields
    );
    info!(
        "runtime config: api_key_env={}, server_info={:?}, http_shutdown={}, verbose_logging={}, feature_flags={:?}",
        config.api_key_env,
        config.server_info,
        config.http_shutdown,
        config.verbose_logging,
        config.feature_flags
    );
    for snapshot in router_manager.get_router_delta_log_snapshots() {
        let RouterDeltaLogSnapshot {
            name,
            active,
            incoming_url,
            upstream_wire,
            override_upstream_url,
            override_upstream_wire,
            override_upstream_http_headers,
            override_forward_incoming_headers,
            override_drop_tool_types,
            override_drop_request_fields,
        } = snapshot;

        let mut overrides = Vec::new();
        if let Some(v) = override_upstream_url {
            overrides.push(format!("upstream_url={v}"));
        }
        if let Some(v) = override_upstream_wire {
            overrides.push(format!("upstream_wire={v:?}"));
        }
        if let Some(v) = override_upstream_http_headers {
            overrides.push(format!("upstream_http_headers={v:?}"));
        }
        if let Some(v) = override_forward_incoming_headers {
            overrides.push(format!("forward_incoming_headers={v:?}"));
        }
        if let Some(v) = override_drop_tool_types {
            overrides.push(format!("drop_tool_types={v:?}"));
        }
        if let Some(v) = override_drop_request_fields {
            overrides.push(format!("drop_request_fields={v:?}"));
        }
        let override_summary = if overrides.is_empty() {
            "none".to_string()
        } else {
            overrides.join(", ")
        };

        info!(
            "router: name={}, active={}, incoming_url={:?}, upstream_wire={:?}, overrides={}",
            name, active, incoming_url, upstream_wire, override_summary
        );
    }

    let state = Arc::new(AppState {
        client,
        api_key,
        http_shutdown: config.http_shutdown,
        verbose_logging: config.verbose_logging,
        routers: Arc::new(RwLock::new(router_manager)),
        sessions: Arc::new(RwLock::new(SessionStore::default())),
    });

    let app = Router::new()
        .route("/v1/responses", post(handle_responses))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/healthz", get(healthz))
        .route("/shutdown", get(shutdown))
        .route("/routers", get(list_routers))
        .route("/profiles", get(list_routers))
        .route("/{*incoming_path}", post(handle_routed_incoming))
        .with_state(state.clone());

    let mut bound_ports = Vec::new();
    let mut join_set = tokio::task::JoinSet::new();
    for bind_addr in &listen_addrs {
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("binding {bind_addr}"))?;
        let local_addr = listener.local_addr().context("reading local_addr")?;
        bound_ports.push(local_addr.port());
        info!("codex-chat-bridge listening on {}", local_addr);

        let app_clone = app.clone();
        let local_addr_for_task = local_addr;
        join_set.spawn(async move {
            axum::serve(listener, app_clone)
                .await
                .with_context(|| format!("serving axum app on {}", local_addr_for_task))
        });
    }

    if let Some(path) = config.server_info.as_ref() {
        write_server_info(path, &bound_ports)?;
    }

    match join_set.join_next().await {
        Some(result) => {
            result.context("listener task failed to join")??;
            Err(anyhow!("listener exited unexpectedly"))
        }
        None => Err(anyhow!("no listener task started")),
    }
}

fn load_file_config(path: &Path) -> Result<Option<FileConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let parsed: FileConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config file {}", path.display()))?;
    info!("loaded config file {}", path.display());
    Ok(Some(parsed))
}

fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_path {
        return Ok(path);
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME environment variable is not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("codex-chat-bridge")
        .join("conf.toml"))
}

fn ensure_default_config_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }

    fs::write(path, DEFAULT_CONFIG_TEMPLATE)
        .with_context(|| format!("creating default config file {}", path.display()))?;
    info!("created default config file {}", path.display());
    Ok(())
}

fn resolve_config(args: Args, file_config: Option<FileConfig>) -> Result<ResolvedConfig> {
    let file_config = file_config.unwrap_or_default();
    let feature_flags = FeatureFlags::default().with_overrides(file_config.features.as_ref());
    let mut drop_tool_types = file_config.drop_tool_types.unwrap_or_default();
    drop_tool_types.extend(args.drop_tool_types);
    drop_tool_types.retain(|v| !v.trim().is_empty());
    let drop_request_fields = normalize_drop_request_fields(file_config.drop_request_fields.unwrap_or_default());
    let selected_upstream_url = args.upstream_url.or(file_config.upstream_url);
    let explicit_upstream_wire = args.upstream_wire.or(file_config.upstream_wire);
    let upstream_wire = resolve_upstream_wire(
        selected_upstream_url.as_deref(),
        explicit_upstream_wire,
        WireApi::Chat,
        "default upstream",
    )?;
    let mut upstream_http_headers = Vec::new();
    for (name, value) in file_config.upstream_http_headers.unwrap_or_default() {
        upsert_upstream_http_header(
            &mut upstream_http_headers,
            validate_upstream_http_header(name, value)?,
        );
    }
    for header in args.upstream_http_headers {
        upsert_upstream_http_header(&mut upstream_http_headers, header);
    }

    let mut forward_incoming_headers = DEFAULT_FORWARDED_UPSTREAM_HEADERS
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>();
    for header in file_config.forward_incoming_headers.unwrap_or_default() {
        upsert_forward_incoming_header(&mut forward_incoming_headers, validate_forward_incoming_header(header)?);
    }
    for header in args.forward_incoming_headers {
        upsert_forward_incoming_header(&mut forward_incoming_headers, validate_forward_incoming_header(header)?);
    }

    Ok(ResolvedConfig {
        upstream_url: selected_upstream_url.unwrap_or_else(|| default_upstream_url(upstream_wire)),
        upstream_wire,
        upstream_http_headers,
        forward_incoming_headers,
        api_key_env: args
            .api_key_env
            .or(file_config.api_key_env)
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        server_info: args.server_info.or(file_config.server_info),
        http_shutdown: args.http_shutdown || file_config.http_shutdown.unwrap_or(false),
        verbose_logging: args.verbose_logging || file_config.verbose_logging.unwrap_or(false),
        drop_tool_types,
        drop_request_fields,
        feature_flags,
    })
}

fn normalize_drop_request_fields(fields: Vec<String>) -> Vec<String> {
    let mut normalized = fields
        .into_iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn parse_upstream_http_header_arg(raw: &str) -> std::result::Result<UpstreamHeader, String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| "expected NAME=VALUE format".to_string())?;
    validate_upstream_http_header(name.to_string(), value.to_string()).map_err(|e| e.to_string())
}

fn validate_upstream_http_header(name: String, value: String) -> Result<UpstreamHeader> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("upstream header name must not be empty"));
    }
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|err| anyhow!("invalid upstream header name `{name}`: {err}"))?;

    let value = value.trim().to_string();
    HeaderValue::from_str(&value)
        .map_err(|err| anyhow!("invalid upstream header value for `{name}`: {err}"))?;

    Ok(UpstreamHeader {
        name: name.to_string(),
        value,
    })
}

fn upsert_upstream_http_header(headers: &mut Vec<UpstreamHeader>, new_header: UpstreamHeader) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|h| h.name.eq_ignore_ascii_case(&new_header.name))
    {
        *existing = new_header;
    } else {
        headers.push(new_header);
    }
}

fn validate_forward_incoming_header(name: String) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("forwarded header name must not be empty"));
    }
    let normalized = trimmed.to_ascii_lowercase();
    HeaderName::from_lowercase(normalized.as_bytes()).with_context(|| {
        format!("invalid forwarded header name `{}`", trimmed)
    })?;
    Ok(normalized)
}

fn upsert_forward_incoming_header(headers: &mut Vec<String>, new_header: String) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|h| h.eq_ignore_ascii_case(&new_header))
    {
        *existing = new_header;
    } else {
        headers.push(new_header);
    }
}

fn default_upstream_url(upstream_wire: WireApi) -> String {
    match upstream_wire {
        WireApi::Chat => "https://api.openai.com/v1/chat/completions".to_string(),
        WireApi::Responses => "https://api.openai.com/v1/responses".to_string(),
    }
}

fn infer_upstream_wire_from_url(raw: &str) -> Option<WireApi> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = reqwest::Url::parse(trimmed)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| trimmed.split('?').next().unwrap_or(trimmed).to_string());
    let normalized = normalize_request_path(&path);

    if normalized.ends_with("/v1/chat/completions") {
        Some(WireApi::Chat)
    } else if normalized.ends_with("/v1/responses") {
        Some(WireApi::Responses)
    } else {
        None
    }
}

fn resolve_upstream_wire(
    upstream_url: Option<&str>,
    explicit_wire: Option<WireApi>,
    fallback_wire: WireApi,
    context: &str,
) -> Result<WireApi> {
    let inferred_wire = upstream_url.and_then(infer_upstream_wire_from_url);
    if let (Some(explicit), Some(inferred)) = (explicit_wire, inferred_wire)
        && explicit != inferred
    {
        return Err(anyhow!(
            "{context} configuration mismatch: upstream_url implies {:?}, but upstream_wire is {:?}",
            inferred,
            explicit
        ));
    }
    Ok(explicit_wire.or(inferred_wire).unwrap_or(fallback_wire))
}

fn normalize_host_port(host: &str, port: u16) -> String {
    let normalized_host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if normalized_host.contains(':') {
        format!("[{}]:{}", normalized_host, port)
    } else {
        format!("{}:{}", normalized_host, port)
    }
}

fn normalize_host_header_to_authority(raw: &str) -> Option<String> {
    let authority: Authority = raw.trim().parse().ok()?;
    let port = authority.port_u16()?;
    Some(normalize_host_port(authority.host(), port))
}

fn describe_route_key(key: &IncomingRouteKey) -> String {
    match key.authority.as_deref() {
        Some(authority) => format!("http://{}{}", authority, key.path),
        None => key.path.clone(),
    }
}

fn parse_incoming_url(raw: &str) -> Result<ParsedIncomingUrl> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("incoming_url must not be empty"));
    }
    let normalized_path = normalize_incoming_url_to_path(trimmed)?;

    if trimmed.starts_with('/') {
        return Ok(ParsedIncomingUrl {
            route_key: IncomingRouteKey {
                authority: None,
                path: normalized_path,
            },
            bind_addr: None,
        });
    }

    let parsed = reqwest::Url::parse(trimmed).map_err(|err| anyhow!("failed to parse URL: {err}"))?;
    if parsed.scheme() != "http" {
        return Err(anyhow!(
            "incoming_url scheme must be `http` for listener binding: {}",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("incoming_url host must not be empty"))?;
    let port = parsed
        .port()
        .ok_or_else(|| anyhow!("incoming_url port must be explicitly specified"))?;
    let authority = normalize_host_port(host, port);

    Ok(ParsedIncomingUrl {
        route_key: IncomingRouteKey {
            authority: Some(authority.clone()),
            path: normalized_path,
        },
        bind_addr: Some(authority),
    })
}

fn normalize_incoming_url_to_path(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("incoming_url must not be empty"));
    }

    let path = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        reqwest::Url::parse(trimmed)
            .map_err(|err| anyhow!("failed to parse URL: {err}"))?
            .path()
            .to_string()
    };

    let path_without_query = path
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(path.as_str())
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(path.as_str());

    Ok(normalize_request_path(path_without_query))
}

fn normalize_request_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }

    let mut normalized = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    };

    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

fn write_server_info(path: &Path, ports: &[u16]) -> Result<()> {
    if ports.is_empty() {
        return Err(anyhow!("no listening ports available"));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let unique_ports = ports.iter().copied().collect::<BTreeSet<_>>();
    let ordered_ports = unique_ports.into_iter().collect::<Vec<_>>();
    let info = ServerInfo {
        port: ordered_ports[0],
        ports: ordered_ports,
        pid: std::process::id(),
    };
    let mut data = serde_json::to_string(&info)?;
    data.push('\n');
    let mut f = File::create(path)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn shutdown(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !state.http_shutdown {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        std::process::exit(0);
    });

    (StatusCode::OK, "shutting down").into_response()
}

async fn list_routers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let routers = state.routers.read().await;
    let router_names = routers.get_router_names();

    json_success_response(json!({
        "routers": router_names,
        "profiles": router_names,
    }))
}

async fn handle_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_incoming(state, headers, body, Some(IncomingApi::Responses), None).await
}

async fn handle_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_incoming(state, headers, body, Some(IncomingApi::Chat), None).await
}

async fn handle_routed_incoming(
    State(state): State<Arc<AppState>>,
    AxumPath(incoming_path): AxumPath<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_incoming(
        state,
        headers,
        body,
        None,
        Some(normalize_request_path(&incoming_path)),
    )
    .await
}

async fn handle_incoming(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: String,
    incoming_api_hint: Option<IncomingApi>,
    incoming_path: Option<String>,
) -> Response {
    let routers = state.routers.read().await;
    let host_header = headers.get(HOST).and_then(|h| h.to_str().ok());
    let route_target = match incoming_path.as_deref() {
        Some(path) => match routers.get_target_for_incoming_route(path, host_header) {
            Ok(Some(target)) => target,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(err) => return json_error_response("router_error", &err.to_string()),
        },
        None => {
            return json_error_response(
                "router_error",
                "routing requires POST /{incoming_path} that matches routers.*.incoming_url",
            )
        }
    };
    let verbose_logging = state.verbose_logging;
    drop(routers);

    let fallback_incoming_api = incoming_api_hint.unwrap_or(IncomingApi::Responses);
    let incoming_route = incoming_path.unwrap_or_else(|| match incoming_api_hint {
        Some(IncomingApi::Chat) => "/v1/chat/completions".to_string(),
        Some(IncomingApi::Responses) => "/v1/responses".to_string(),
        None => "/v1/responses".to_string(),
    });

    debug!(
        "request routed: router={}, incoming_route={}, upstream_url={}, upstream_wire={:?}",
        route_target.router_name,
        incoming_route,
        route_target.upstream_url,
        route_target.upstream_wire
    );

    if verbose_logging {
        debug!(
            "incoming headers (router={}): {}",
            route_target.router_name,
            headers_for_logging(&headers)
        );
        debug!(
            "incoming request body (router={}): {body}",
            route_target.router_name
        );
    }

    let mut request_value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(err) => {
            return error_response_for_stream(
                stream_default_for_api(fallback_incoming_api),
                "invalid_request",
                &format!("failed to parse request JSON: {err}"),
            )
        }
    };
    if verbose_logging {
        debug!(
            "incoming tool types (router={}): {}",
            route_target.router_name,
            tool_types_for_logging(&request_value)
        );
        debug!(
            "incoming request fields (router={}): {}",
            route_target.router_name,
            request_fields_for_logging(&request_value)
        );
    }

    let incoming_api = incoming_api_hint.unwrap_or_else(|| infer_incoming_api(&request_value));
    let wants_stream = stream_flag_for_request(incoming_api, &request_value);
    apply_request_filters(
        incoming_api,
        &mut request_value,
        &route_target.drop_tool_types,
        &route_target.drop_request_fields,
    );
    if let Err(err) = validate_capability_gate(
        incoming_api,
        route_target.upstream_wire,
        route_target.feature_flags.enable_extended_input_types,
        &request_value,
    ) {
        return error_response_for_stream(wants_stream, "unsupported_feature", &err.to_string());
    }
    let tool_call_kinds_by_name = if incoming_api == IncomingApi::Responses {
        responses_tool_call_kind_by_name(&request_value)
    } else {
        HashMap::new()
    };

    let response_id = format!("resp_bridge_{}", Uuid::now_v7());
    let mut upstream_payload =
        match build_upstream_payload(
            &request_value,
            incoming_api,
            route_target.upstream_wire,
            wants_stream,
            route_target.feature_flags.enable_extended_input_types,
        )
        {
            Ok(v) => v,
            Err(err) => return error_response_for_stream(wants_stream, "invalid_request", &err.to_string()),
        };

    if route_target.feature_flags.enable_previous_response_id
        && incoming_api == IncomingApi::Responses
        && route_target.upstream_wire == WireApi::Chat
    {
        let previous_messages = {
            let sessions = state.sessions.read().await;
            match resolve_previous_messages_for_request(&request_value, &sessions) {
                Ok(v) => v,
                Err(err) => {
                    return error_response_for_stream(
                        wants_stream,
                        "invalid_request",
                        &err.to_string(),
                    )
                }
            }
        };
        if let Some(messages) = previous_messages
            && let Err(err) = merge_previous_messages(&mut upstream_payload, &messages)
        {
            return error_response_for_stream(wants_stream, "invalid_request", &err.to_string());
        }

        let chat_messages = upstream_payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut sessions = state.sessions.write().await;
        sessions.insert_messages(response_id.clone(), chat_messages);
    }

    if verbose_logging {
        if let Some(messages) =
            upstream_messages_for_logging(route_target.upstream_wire, &upstream_payload)
        {
            debug!(
                "upstream messages (router={}, {:?}->{:?}): {}",
                route_target.router_name, incoming_api, route_target.upstream_wire, messages
            );
        }

        debug!(
            "upstream headers (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            upstream_headers_for_logging(
                &headers,
                &state.api_key,
                &route_target.upstream_http_headers,
                &route_target.forward_incoming_headers,
            )
        );

        debug!(
            "upstream payload (router={}, {:?}->{:?}): {}",
            route_target.router_name, incoming_api, route_target.upstream_wire, upstream_payload
        );
        debug!(
            "upstream tool types (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            tool_types_for_logging(&upstream_payload)
        );
        debug!(
            "upstream request fields (router={}, {:?}->{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            request_fields_for_logging(&upstream_payload)
        );
    }

    let mut upstream_request = state
        .client
        .post(&route_target.upstream_url)
        .bearer_auth(&state.api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&upstream_payload);

    for header_name in &route_target.forward_incoming_headers {
        if let Some(value) = headers.get(header_name) {
            upstream_request = upstream_request.header(header_name, value.clone());
        }
    }
    for header in &route_target.upstream_http_headers {
        upstream_request = upstream_request.header(&header.name, &header.value);
    }

    let upstream_response = match upstream_request.send().await {
        Ok(response) => response,
        Err(err) => {
            return error_response_for_stream(
                wants_stream,
                "upstream_transport_error",
                &format!("failed to call upstream endpoint: {err}"),
            )
        }
    };

    if verbose_logging {
        debug!(
            "upstream response status (router={}): {} {}",
            route_target.router_name,
            upstream_response.status().as_u16(),
            upstream_response.status()
        );
        debug!(
            "upstream response headers (router={}, {:?}<-{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            headers_for_logging(upstream_response.headers())
        );
    }

    if !upstream_response.status().is_success() {
        let status = upstream_response.status();
        let body = upstream_response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        if verbose_logging {
            debug!(
                "upstream response payload error (router={}, {:?}<-{:?}): {body}",
                route_target.router_name,
                incoming_api,
                route_target.upstream_wire
            );
        }
        let normalized = normalize_upstream_error_payload(status, &body);
        return error_response_for_stream(wants_stream, &normalized.code, &normalized.message);
    }

    if wants_stream {
        let body = match route_target.upstream_wire {
            WireApi::Chat => Body::from_stream(translate_chat_stream(
                upstream_response.bytes_stream(),
                response_id,
                route_target.router_name.clone(),
                verbose_logging,
                tool_call_kinds_by_name.clone(),
                route_target.feature_flags,
            )),
            WireApi::Responses => Body::from_stream(passthrough_responses_stream(
                upstream_response.bytes_stream(),
                route_target.router_name.clone(),
                verbose_logging,
            )),
        };

        return (
            StatusCode::OK,
            [
                (CONTENT_TYPE, HeaderValue::from_static("text/event-stream")),
                (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
                (
                    HeaderName::from_static("x-accel-buffering"),
                    HeaderValue::from_static("no"),
                ),
            ],
            body,
        )
            .into_response();
    }

    let upstream_json = match upstream_response.json::<Value>().await {
        Ok(v) => v,
        Err(err) => {
            return json_error_response(
                "upstream_decode_error",
                &format!("failed to decode upstream JSON: {err}"),
            )
        }
    };
    if verbose_logging {
        debug!(
            "upstream response payload (router={}, {:?}<-{:?}): {}",
            route_target.router_name,
            incoming_api,
            route_target.upstream_wire,
            upstream_json
        );
    }

    let response_json = match route_target.upstream_wire {
        WireApi::Chat => {
            chat_json_to_responses_json(
                upstream_json,
                response_id,
                &tool_call_kinds_by_name,
                route_target.feature_flags.enable_provider_specific_fields,
            )
        }
        WireApi::Responses => upstream_json,
    };

    json_success_response(response_json)
}

fn infer_incoming_api(request: &Value) -> IncomingApi {
    if request.get("messages").is_some() {
        IncomingApi::Chat
    } else {
        IncomingApi::Responses
    }
}

fn stream_default_for_api(incoming_api: IncomingApi) -> bool {
    match incoming_api {
        IncomingApi::Responses => true,
        IncomingApi::Chat => false,
    }
}

fn stream_flag_for_request(incoming_api: IncomingApi, request: &Value) -> bool {
    request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| stream_default_for_api(incoming_api))
}

fn apply_request_filters(
    incoming_api: IncomingApi,
    request: &mut Value,
    drop_tool_types: &HashSet<String>,
    drop_request_fields: &HashSet<String>,
) {
    let Some(obj) = request.as_object_mut() else {
        return;
    };

    for field in drop_request_fields {
        obj.remove(field);
    }

    let Some(tools) = obj.get_mut("tools") else {
        return;
    };

    let Some(items) = tools.as_array_mut() else {
        return;
    };

    items.retain(|tool| {
        let tool_type = match incoming_api {
            IncomingApi::Responses => tool.get("type").and_then(Value::as_str),
            IncomingApi::Chat => tool.get("type").and_then(Value::as_str),
        };
        !tool_type.is_some_and(|t| drop_tool_types.contains(t))
    });

    if items.is_empty() {
        obj.remove("tools");
        obj.remove("tool_choice");
    }
}

fn validate_capability_gate(
    incoming_api: IncomingApi,
    upstream_wire: WireApi,
    enable_extended_input_types: bool,
    request: &Value,
) -> Result<()> {
    if incoming_api != IncomingApi::Responses || upstream_wire != WireApi::Chat {
        return Ok(());
    }

    for field in ["reasoning", "include", "text", "service_tier"] {
        if request_has_non_empty_field(request, field) {
            return Err(anyhow!(
                "responses->chat bridge does not support `{field}` yet; disable it or route to native responses upstream"
            ));
        }
    }

    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or_default();
            let is_supported = if enable_extended_input_types {
                matches!(
                    tool_type,
                    "function" | "custom" | "mcp" | "web_search" | "web_search_preview"
                )
            } else {
                matches!(tool_type, "function" | "custom")
            };
            if !is_supported {
                return Err(anyhow!(
                    "responses->chat bridge does not support tool type `{tool_type}`"
                ));
            }
        }
    }

    Ok(())
}

fn request_has_non_empty_field(request: &Value, field: &str) -> bool {
    let Some(value) = request.get(field) else {
        return false;
    };

    match value {
        Value::Null => false,
        Value::String(v) => !v.trim().is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
        Value::Bool(v) => *v,
        Value::Number(_) => true,
    }
}

fn previous_response_id_for_request(request: &Value) -> Option<&str> {
    request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn resolve_previous_messages_for_request(
    request: &Value,
    sessions: &SessionStore,
) -> Result<Option<Vec<Value>>> {
    let Some(previous_response_id) = previous_response_id_for_request(request) else {
        return Ok(None);
    };

    let Some(messages) = sessions.get_messages(previous_response_id) else {
        return Err(anyhow!(
            "unknown `previous_response_id`: {previous_response_id}"
        ));
    };
    Ok(Some(messages))
}

fn merge_previous_messages(upstream_payload: &mut Value, previous_messages: &[Value]) -> Result<()> {
    let obj = upstream_payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("upstream chat payload must be an object"))?;
    let messages = obj
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("upstream chat payload is missing `messages` array"))?;
    if previous_messages.is_empty() {
        return Ok(());
    }
    let mut merged = previous_messages.to_vec();
    merged.extend(messages.iter().cloned());
    *messages = merged;
    Ok(())
}

fn build_upstream_payload(
    request: &Value,
    incoming_api: IncomingApi,
    upstream_wire: WireApi,
    stream: bool,
    enable_extended_input_types: bool,
) -> Result<Value> {
    let mut payload = match (incoming_api, upstream_wire) {
        (IncomingApi::Responses, WireApi::Responses) => request.clone(),
        (IncomingApi::Responses, WireApi::Chat) => {
            map_responses_to_chat_request_with_stream(
                request,
                &HashSet::new(),
                stream,
                enable_extended_input_types,
            )?
            .chat_request
        }
        (IncomingApi::Chat, WireApi::Chat) => request.clone(),
        (IncomingApi::Chat, WireApi::Responses) => map_chat_to_responses_request(request, stream)?,
    };
    set_stream_flag(&mut payload, stream);
    Ok(payload)
}

fn normalize_upstream_error_payload(status: StatusCode, body: &str) -> NormalizedUpstreamError {
    let default_message = format!("upstream returned {status}: {body}");
    let mut upstream_code = String::new();
    let mut upstream_message = String::new();

    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        if let Some(error) = parsed.get("error") {
            if let Some(code) = error.get("code").and_then(Value::as_str) {
                upstream_code = code.to_string();
            }
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                upstream_message = message.to_string();
            }
        } else if let Some(code) = parsed.get("code").and_then(Value::as_str) {
            upstream_code = code.to_string();
            if let Some(message) = parsed.get("message").and_then(Value::as_str) {
                upstream_message = message.to_string();
            }
        }
    }

    let normalized_code = match upstream_code.as_str() {
        "context_length_exceeded" => "context_window_exceeded",
        "insufficient_quota" => "quota_exceeded",
        "rate_limit_exceeded" => "rate_limit_exceeded",
        "invalid_request_error" | "invalid_request" => "invalid_request",
        _ => {
            if status == StatusCode::TOO_MANY_REQUESTS {
                "rate_limit_exceeded"
            } else if status == StatusCode::BAD_REQUEST {
                "invalid_request"
            } else {
                "upstream_error"
            }
        }
    };
    let normalized_message = if upstream_message.is_empty() {
        default_message
    } else {
        upstream_message
    };

    NormalizedUpstreamError {
        code: normalized_code.to_string(),
        message: normalized_message,
    }
}

fn set_stream_flag(payload: &mut Value, stream: bool) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("stream".to_string(), Value::Bool(stream));
    }
}

fn upstream_messages_for_logging(upstream_wire: WireApi, payload: &Value) -> Option<Value> {
    match upstream_wire {
        WireApi::Chat => payload.get("messages").cloned(),
        WireApi::Responses => payload
            .get("input")
            .and_then(Value::as_array)
            .map(|items| {
                let messages = items
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                    .cloned()
                    .collect();
                Value::Array(messages)
            }),
    }
}

fn tool_types_for_logging(payload: &Value) -> Value {
    let tool_labels = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(tool_type_label_for_logging)
                .map(Value::String)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(tool_labels)
}

fn request_fields_for_logging(payload: &Value) -> Value {
    let Some(obj) = payload.as_object() else {
        return Value::Array(Vec::new());
    };

    let mut fields = obj.keys().cloned().collect::<Vec<_>>();
    fields.sort();
    Value::Array(fields.into_iter().map(Value::String).collect())
}

fn tool_type_label_for_logging(tool: &Value) -> String {
    let Some(obj) = tool.as_object() else {
        return "<invalid_tool>".to_string();
    };

    let tool_type = obj
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let function_name = obj
        .get("function")
        .and_then(Value::as_object)
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let top_level_name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let tool_name = function_name.or(top_level_name);

    match (tool_type, tool_name) {
        (Some(t), Some(n)) => format!("{t}({n})"),
        (Some(t), None) => t.to_string(),
        (None, Some(n)) => format!("<missing_type>({n})"),
        (None, None) => "<missing_type>".to_string(),
    }
}

fn upstream_headers_for_logging(
    headers: &HeaderMap,
    api_key: &str,
    upstream_http_headers: &[UpstreamHeader],
    forwarded_headers: &[String],
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "authorization".to_string(),
        Value::String(format!("Bearer {}", redact_for_logging(api_key))),
    );
    out.insert(
        CONTENT_TYPE.as_str().to_string(),
        Value::String("application/json".to_string()),
    );

    for header_name in forwarded_headers {
        if let Some(value) = headers.get(header_name) {
            let header_value = value
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|_| "<non-utf8>".to_string());
            out.insert(header_name.to_string(), Value::String(header_value));
        }
    }
    for header in upstream_http_headers {
        let header_name = header.name.to_ascii_lowercase();
        let header_value = if is_sensitive_upstream_header(&header_name) {
            redact_for_logging(&header.value).to_string()
        } else {
            header.value.clone()
        };
        out.insert(header_name, Value::String(header_value));
    }

    Value::Object(out)
}

fn headers_for_logging(headers: &HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        let header_name = name.as_str().to_ascii_lowercase();
        let header_value = if is_sensitive_upstream_header(&header_name)
            || header_name.eq_ignore_ascii_case("cookie")
            || header_name.eq_ignore_ascii_case("set-cookie")
        {
            redact_for_logging(
                value
                    .to_str()
                    .ok()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("x"),
            )
            .to_string()
        } else {
            value
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|_| "<non-utf8>".to_string())
        };
        out.insert(header_name, Value::String(header_value));
    }
    Value::Object(out)
}

fn is_sensitive_upstream_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("api-key")
}

fn redact_for_logging(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

fn sse_error_response(code: &str, message: &str) -> Response {
    let response_id = format!("resp_bridge_{}", Uuid::now_v7());
    let mut body = Vec::new();
    body.extend_from_slice(&sse_event(
        "response.created",
        &json!({
            "type": "response.created",
            "response": {
                "id": response_id.clone(),
            }
        }),
    ));
    body.extend_from_slice(&sse_event(
        "response.failed",
        &json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "error": {
                    "code": code,
                    "message": message,
                }
            }
        }),
    ));

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("text/event-stream")),
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

fn error_response_for_stream(stream: bool, code: &str, message: &str) -> Response {
    if stream {
        sse_error_response(code, message)
    } else {
        json_error_response(code, message)
    }
}

fn json_success_response(payload: Value) -> Response {
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    (
        StatusCode::OK,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    )
        .into_response()
}

fn json_error_response(code: &str, message: &str) -> Response {
    json_success_response(json!({
        "error": {
            "type": code,
            "message": message,
        }
    }))
}


#[cfg(test)]
mod tests;
