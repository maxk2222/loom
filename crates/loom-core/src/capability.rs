use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// API type of a virtual model / request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiType {
    Chat,
    Completions,
    Embeddings,
    /// Virtual model that serves more than one api type (resolved per request).
    Multi,
}

impl ApiType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiType::Chat => "chat",
            ApiType::Completions => "completions",
            ApiType::Embeddings => "embeddings",
            ApiType::Multi => "multi",
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            ApiType::Chat => "/v1/chat/completions",
            ApiType::Completions => "/v1/completions",
            ApiType::Embeddings => "/v1/embeddings",
            ApiType::Multi => "/v1",
        }
    }
}

impl std::fmt::Display for ApiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ApiType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "chat" => ApiType::Chat,
            "completions" => ApiType::Completions,
            "embeddings" => ApiType::Embeddings,
            "multi" => ApiType::Multi,
            _ => return Err(()),
        })
    }
}

/// Capabilities of a single deployment (what the backend/model can do).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub chat: bool,
    #[serde(default)]
    pub completions: bool,
    #[serde(default)]
    pub embeddings: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub json_mode: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub context_length: u32,
    #[serde(default)]
    pub modalities: BTreeSet<String>,
}

impl Capabilities {
    /// OpenAI-style backends generally support all endpoints of the OpenAI
    /// surface they implement; probing refines tools/vision/json_mode.
    pub fn supports_api(&self, api: ApiType) -> bool {
        match api {
            ApiType::Chat => self.chat,
            ApiType::Completions => self.completions,
            ApiType::Embeddings => self.embeddings,
            ApiType::Multi => self.chat || self.completions || self.embeddings,
        }
    }

    /// Check whether this deployment satisfies a required-caps request.
    pub fn satisfies(&self, req: &RequiredCaps) -> bool {
        (!req.tools || self.tools)
            && (!req.vision || self.vision)
            && (!req.streaming || self.streaming)
            && (!req.json_mode || self.json_mode)
    }
}

/// Requirements derived from an incoming OpenAI request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequiredCaps {
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub json_mode: bool,
}

/// Backend implementation families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    Vllm,
    Ollama,
    GenericOpenAi,
}

impl BackendType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendType::Vllm => "vllm",
            BackendType::Ollama => "ollama",
            BackendType::GenericOpenAi => "generic_openai",
        }
    }
}

impl std::fmt::Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BackendType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "vllm" => BackendType::Vllm,
            "ollama" => BackendType::Ollama,
            "generic_openai" | "openai" | "generic" => BackendType::GenericOpenAi,
            _ => return Err(()),
        })
    }
}

/// Sharing mode of a deployment toward the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Never offered to the cluster.
    #[default]
    Private,
    /// Share configured free capacity opportunistically.
    Opportunistic,
    /// Reserved local capacity, remaining shareable.
    Shared,
}

impl SharingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SharingMode::Private => "private",
            SharingMode::Opportunistic => "opportunistic",
            SharingMode::Shared => "shared",
        }
    }
}

impl std::fmt::Display for SharingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SharingMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "private" => SharingMode::Private,
            "opportunistic" => SharingMode::Opportunistic,
            "shared" | "reserved" => SharingMode::Shared,
            _ => return Err(()),
        })
    }
}
