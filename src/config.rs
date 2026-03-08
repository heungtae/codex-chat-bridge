use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use clap::Parser;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tracing::info;

use crate::model::DEFAULT_FORWARDED_UPSTREAM_HEADERS;
use crate::model::FeatureFlags;
use crate::model::FeatureFlagsConfig;
use crate::model::UpstreamHeader;
use crate::model::WireApi;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "codex-chat-bridge",
    about = "Responses-to-Chat completions bridge",
    version
)]
pub(crate) struct Args {
    #[arg(
        long,
        value_name = "FILE",
        help = "config file path (default: ~/.config/codex-chat-bridge/conf.toml)"
    )]
    pub(crate) config: Option<PathBuf>,

    #[arg(long)]
    pub(crate) upstream_url: Option<String>,

    #[arg(long, value_enum)]
    pub(crate) upstream_wire: Option<WireApi>,

    #[arg(
        long = "upstream-http-header",
        value_name = "NAME=VALUE",
        action = clap::ArgAction::Append,
        value_parser = parse_upstream_http_header_arg,
        help = "add static header sent to upstream requests; can be repeated"
    )]
    pub(crate) upstream_http_headers: Vec<UpstreamHeader>,

    #[arg(
        long = "forward-incoming-header",
        value_name = "NAME",
        action = clap::ArgAction::Append,
        help = "forward this incoming header onto the upstream request; can be repeated"
    )]
    pub(crate) forward_incoming_headers: Vec<String>,

    #[arg(long)]
    pub(crate) api_key_env: Option<String>,

    #[arg(long, value_name = "FILE")]
    pub(crate) server_info: Option<PathBuf>,

    #[arg(long)]
    pub(crate) http_shutdown: bool,

    #[arg(long, help = "enable verbose bridge logs (request/response payloads)")]
    pub(crate) verbose_logging: bool,

    #[arg(
        long = "drop-tool-type",
        value_name = "TYPE",
        action = clap::ArgAction::Append,
        help = "drop tool entries whose `type` matches this value; can be repeated"
    )]
    pub(crate) drop_tool_types: Vec<String>,

    #[arg(
        long,
        short = 'r',
        alias = "profile",
        help = "use router from config file"
    )]
    pub(crate) router: Option<String>,

    #[arg(
        long,
        alias = "list-profiles",
        help = "list available routers from config file and exit"
    )]
    pub(crate) list_routers: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct FileConfig {
    pub(crate) upstream_url: Option<String>,
    pub(crate) upstream_wire: Option<WireApi>,
    #[serde(alias = "http_headers")]
    pub(crate) upstream_http_headers: Option<BTreeMap<String, String>>,
    pub(crate) forward_incoming_headers: Option<Vec<String>>,
    pub(crate) api_key_env: Option<String>,
    pub(crate) server_info: Option<PathBuf>,
    pub(crate) http_shutdown: Option<bool>,
    pub(crate) verbose_logging: Option<bool>,
    pub(crate) drop_tool_types: Option<Vec<String>>,
    pub(crate) drop_request_fields: Option<Vec<String>>,
    pub(crate) features: Option<FeatureFlagsConfig>,
    #[serde(alias = "profiles")]
    pub(crate) routers: Option<BTreeMap<String, RouterConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RouterConfig {
    pub(crate) upstream_url: Option<String>,
    pub(crate) upstream_wire: Option<WireApi>,
    #[serde(alias = "http_headers")]
    pub(crate) upstream_http_headers: Option<BTreeMap<String, String>>,
    pub(crate) forward_incoming_headers: Option<Vec<String>>,
    pub(crate) drop_tool_types: Option<Vec<String>>,
    pub(crate) drop_request_fields: Option<Vec<String>>,
    pub(crate) features: Option<FeatureFlagsConfig>,
    pub(crate) incoming_url: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedConfig {
    pub(crate) upstream_url: String,
    pub(crate) upstream_wire: WireApi,
    pub(crate) upstream_http_headers: Vec<UpstreamHeader>,
    pub(crate) forward_incoming_headers: Vec<String>,
    pub(crate) api_key_env: String,
    pub(crate) server_info: Option<PathBuf>,
    pub(crate) http_shutdown: bool,
    pub(crate) verbose_logging: bool,
    pub(crate) drop_tool_types: Vec<String>,
    pub(crate) drop_request_fields: Vec<String>,
    pub(crate) feature_flags: FeatureFlags,
}

pub(crate) const DEFAULT_CONFIG_TEMPLATE: &str = r#"# codex-chat-bridge runtime configuration
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

pub(crate) fn load_file_config(path: &Path) -> Result<Option<FileConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let parsed: FileConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config file {}", path.display()))?;
    info!("loaded config file {}", path.display());
    Ok(Some(parsed))
}

pub(crate) fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
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

pub(crate) fn ensure_default_config_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }

    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
        .with_context(|| format!("creating default config file {}", path.display()))?;
    info!("created default config file {}", path.display());
    Ok(())
}

pub(crate) fn resolve_config(args: Args, file_config: Option<FileConfig>) -> Result<ResolvedConfig> {
    let file_config = file_config.unwrap_or_default();
    let feature_flags = FeatureFlags::default().with_overrides(file_config.features.as_ref());
    let mut drop_tool_types = file_config.drop_tool_types.unwrap_or_default();
    drop_tool_types.extend(args.drop_tool_types);
    drop_tool_types.retain(|v| !v.trim().is_empty());
    let drop_request_fields =
        normalize_drop_request_fields(file_config.drop_request_fields.unwrap_or_default());
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
        upsert_forward_incoming_header(
            &mut forward_incoming_headers,
            validate_forward_incoming_header(header)?,
        );
    }
    for header in args.forward_incoming_headers {
        upsert_forward_incoming_header(
            &mut forward_incoming_headers,
            validate_forward_incoming_header(header)?,
        );
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

pub(crate) fn parse_upstream_http_header_arg(raw: &str) -> std::result::Result<UpstreamHeader, String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| "expected NAME=VALUE format".to_string())?;
    validate_upstream_http_header(name.to_string(), value.to_string()).map_err(|e| e.to_string())
}

pub(crate) fn validate_upstream_http_header(name: String, value: String) -> Result<UpstreamHeader> {
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

pub(crate) fn upsert_upstream_http_header(
    headers: &mut Vec<UpstreamHeader>,
    new_header: UpstreamHeader,
) {
    if let Some(existing) = headers
        .iter_mut()
        .find(|h| h.name.eq_ignore_ascii_case(&new_header.name))
    {
        *existing = new_header;
    } else {
        headers.push(new_header);
    }
}

pub(crate) fn validate_forward_incoming_header(name: String) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("forwarded header name must not be empty"));
    }
    let normalized = trimmed.to_ascii_lowercase();
    HeaderName::from_lowercase(normalized.as_bytes())
        .with_context(|| format!("invalid forwarded header name `{}`", trimmed))?;
    Ok(normalized)
}

pub(crate) fn upsert_forward_incoming_header(headers: &mut Vec<String>, new_header: String) {
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
    let normalized = normalize_request_path(path.as_str());

    if normalized.ends_with("/v1/chat/completions") {
        Some(WireApi::Chat)
    } else if normalized.ends_with("/v1/responses") {
        Some(WireApi::Responses)
    } else {
        None
    }
}

pub(crate) fn resolve_upstream_wire(
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
