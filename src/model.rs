use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WireApi {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolTransformMode {
    Passthrough,
    LegacyConvert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncomingApi {
    Responses,
    Chat,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamHeader {
    pub(crate) name: String,
    pub(crate) value: String,
}

pub(crate) const DEFAULT_FORWARDED_UPSTREAM_HEADERS: [&str; 6] = [
    "openai-organization",
    "openai-project",
    "x-client-request-id",
    "x-openai-subagent",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
];

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct FeatureFlagsConfig {
    pub(crate) enable_previous_response_id: Option<bool>,
    pub(crate) enable_tool_argument_stream_events: Option<bool>,
    pub(crate) enable_extended_stream_events: Option<bool>,
    pub(crate) enable_reasoning_stream_events: Option<bool>,
    pub(crate) enable_provider_specific_fields: Option<bool>,
    pub(crate) enable_extended_input_types: Option<bool>,
    pub(crate) tool_transform_mode: Option<ToolTransformMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FeatureFlags {
    pub(crate) enable_previous_response_id: bool,
    pub(crate) enable_tool_argument_stream_events: bool,
    pub(crate) enable_extended_stream_events: bool,
    pub(crate) enable_reasoning_stream_events: bool,
    pub(crate) enable_provider_specific_fields: bool,
    pub(crate) enable_extended_input_types: bool,
    pub(crate) tool_transform_mode: ToolTransformMode,
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
            tool_transform_mode: ToolTransformMode::LegacyConvert,
        }
    }
}

impl FeatureFlags {
    pub(crate) fn with_overrides(mut self, overrides: Option<&FeatureFlagsConfig>) -> Self {
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
        if let Some(v) = overrides.tool_transform_mode {
            self.tool_transform_mode = v;
        }
        self
    }
}
