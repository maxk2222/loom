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
    /// POST /v1/images/generations (JSON).
    Images,
    /// POST /v1/audio/speech — text-to-speech (JSON).
    AudioSpeech,
    /// POST /v1/audio/transcriptions — speech-to-text (multipart).
    AudioTranscription,
    /// POST /v1/audio/translations (multipart).
    AudioTranslation,
    /// POST /v1/videos (JSON passthrough; job-style polling not yet routed).
    Video,
}

impl ApiType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiType::Chat => "chat",
            ApiType::Completions => "completions",
            ApiType::Embeddings => "embeddings",
            ApiType::Multi => "multi",
            ApiType::Images => "images",
            ApiType::AudioSpeech => "audio_speech",
            ApiType::AudioTranscription => "audio_transcription",
            ApiType::AudioTranslation => "audio_translation",
            ApiType::Video => "video",
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            ApiType::Chat => "/v1/chat/completions",
            ApiType::Completions => "/v1/completions",
            ApiType::Embeddings => "/v1/embeddings",
            ApiType::Multi => "/v1",
            ApiType::Images => "/v1/images/generations",
            ApiType::AudioSpeech => "/v1/audio/speech",
            ApiType::AudioTranscription => "/v1/audio/transcriptions",
            ApiType::AudioTranslation => "/v1/audio/translations",
            ApiType::Video => "/v1/videos",
        }
    }

    /// Text-family apis where an unprobed deployment is admitted permissively.
    /// Media apis require an explicit capability flag — a never-probed text
    /// deployment must never absorb e.g. an image-generation request.
    pub fn is_text_family(&self) -> bool {
        matches!(
            self,
            ApiType::Chat | ApiType::Completions | ApiType::Embeddings | ApiType::Multi
        )
    }

    /// Whether the request body is multipart/form-data rather than JSON.
    pub fn is_multipart(&self) -> bool {
        matches!(
            self,
            ApiType::AudioTranscription | ApiType::AudioTranslation
        )
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
            "images" => ApiType::Images,
            "audio_speech" => ApiType::AudioSpeech,
            "audio_transcription" => ApiType::AudioTranscription,
            "audio_translation" => ApiType::AudioTranslation,
            "video" => ApiType::Video,
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
    /// POST /v1/images/generations support.
    #[serde(default)]
    pub images: bool,
    /// POST /v1/audio/speech (TTS).
    #[serde(default)]
    pub audio_speech: bool,
    /// POST /v1/audio/transcriptions (STT).
    #[serde(default)]
    pub audio_transcription: bool,
    /// POST /v1/audio/translations.
    #[serde(default)]
    pub audio_translation: bool,
    /// POST /v1/videos.
    #[serde(default)]
    pub video: bool,
    /// Chat video *input* (video_url / input_video parts) — multimodal
    /// video understanding, not generation. Also implied by a "video"
    /// entry in `modalities`.
    #[serde(default)]
    pub video_input: bool,
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
            ApiType::Images => self.images,
            ApiType::AudioSpeech => self.audio_speech,
            ApiType::AudioTranscription => self.audio_transcription,
            ApiType::AudioTranslation => self.audio_translation,
            ApiType::Video => self.video,
        }
    }

    /// Check whether this deployment satisfies a required-caps request.
    pub fn satisfies(&self, req: &RequiredCaps) -> bool {
        (!req.tools || self.tools)
            && (!req.vision || self.vision)
            && (!req.streaming || self.streaming)
            && (!req.json_mode || self.json_mode)
            && (!req.video || self.video_input || self.modalities.contains("video"))
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
    /// Content parts carrying video input (video_url / input_video) —
    /// routes only to multimodal deployments that declare video_input or
    /// a "video" modality.
    #[serde(default)]
    pub video: bool,
}

/// Backend implementation families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    Vllm,
    Ollama,
    GenericOpenAi,
    /// Azure OpenAI: `api-key` header + /openai/deployments/{dep} URL scheme.
    AzureOpenAi,
    /// Anthropic Messages API (translated to/from OpenAI chat format).
    Anthropic,
    /// llama.cpp server: OpenAI-compat + native model load/unload.
    LlamaCpp,
}

impl BackendType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendType::Vllm => "vllm",
            BackendType::Ollama => "ollama",
            BackendType::GenericOpenAi => "generic_openai",
            BackendType::AzureOpenAi => "azure_openai",
            BackendType::Anthropic => "anthropic",
            BackendType::LlamaCpp => "llama_cpp",
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
