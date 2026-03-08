use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use axum::http::uri::Authority;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use crate::config::RouterConfig;
use crate::config::resolve_upstream_wire;
use crate::config::upsert_upstream_http_header;
use crate::config::validate_forward_incoming_header;
use crate::model::DEFAULT_FORWARDED_UPSTREAM_HEADERS;
use crate::model::FeatureFlags;
use crate::model::UpstreamHeader;
use crate::model::WireApi;

#[derive(Clone)]
pub(crate) struct RouterManager {
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
pub(crate) struct RouteTarget {
    pub(crate) router_name: String,
    pub(crate) upstream_url: String,
    pub(crate) upstream_wire: WireApi,
    pub(crate) upstream_http_headers: Vec<UpstreamHeader>,
    pub(crate) forward_incoming_headers: Vec<String>,
    pub(crate) drop_tool_types: HashSet<String>,
    pub(crate) drop_request_fields: HashSet<String>,
    pub(crate) feature_flags: FeatureFlags,
}

#[derive(Clone, Debug)]
pub(crate) struct RouterDefaultsLogSnapshot {
    pub(crate) upstream_url: String,
    pub(crate) upstream_wire: WireApi,
    pub(crate) upstream_http_headers: Vec<UpstreamHeader>,
    pub(crate) forward_incoming_headers: Vec<String>,
    pub(crate) drop_tool_types: Vec<String>,
    pub(crate) drop_request_fields: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RouterDeltaLogSnapshot {
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) incoming_url: Option<String>,
    pub(crate) upstream_wire: WireApi,
    pub(crate) override_upstream_url: Option<String>,
    pub(crate) override_upstream_wire: Option<WireApi>,
    pub(crate) override_upstream_http_headers: Option<Vec<UpstreamHeader>>,
    pub(crate) override_forward_incoming_headers: Option<Vec<String>>,
    pub(crate) override_drop_tool_types: Option<Vec<String>>,
    pub(crate) override_drop_request_fields: Option<Vec<String>>,
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
    pub(crate) fn new(
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

    pub(crate) fn get_router_names(&self) -> Vec<String> {
        self.routers.keys().cloned().collect()
    }

    pub(crate) fn get_default_log_snapshot(&self) -> RouterDefaultsLogSnapshot {
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

    pub(crate) fn get_router_delta_log_snapshots(&self) -> Vec<RouterDeltaLogSnapshot> {
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
            let override_upstream_wire =
                (resolved_upstream_wire != self.default_upstream_wire).then_some(resolved_upstream_wire);

            let override_upstream_http_headers =
                router_cfg
                    .upstream_http_headers
                    .as_ref()
                    .and_then(|router_headers| {
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
                    });

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

    pub(crate) fn get_listen_addrs(&self) -> Vec<String> {
        self.listen_addrs.iter().cloned().collect()
    }

    pub(crate) fn get_target_for_incoming_route(
        &self,
        path: &str,
        host_header: Option<&str>,
    ) -> Result<Option<RouteTarget>> {
        let normalized_path = normalize_request_path(path);
        if let Some(authority) = host_header.and_then(normalize_host_header_to_authority) {
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

pub(crate) fn normalize_incoming_url_to_path(raw: &str) -> Result<String> {
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

pub(crate) fn normalize_request_path(path: &str) -> String {
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
