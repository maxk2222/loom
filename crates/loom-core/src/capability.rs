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

    /// Apis an unprobed (all-false capabilities) deployment may serve
    /// optimistically — local client admission is authoritative for text
    /// generation only. Embeddings and every media api require a probed or
    /// declared flag: unlike chat, an embedding request against a backend
    /// without an embedding model cannot fall back and only fails upstream,
    /// so in an omni/multi virtual model a freshly registered chat
    /// deployment would otherwise steal embedding traffic until probed.
    pub fn admits_unprobed(&self) -> bool {
        matches!(self, ApiType::Chat | ApiType::Completions | ApiType::Multi)
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

    /// Merge another deployment's capabilities into self: flags OR —
    /// "the model can do X when at least one member can" — context takes
    /// the largest offered window, modalities union.
    pub fn union_with(&mut self, o: &Capabilities) {
        self.chat |= o.chat;
        self.completions |= o.completions;
        self.embeddings |= o.embeddings;
        self.images |= o.images;
        self.audio_speech |= o.audio_speech;
        self.audio_transcription |= o.audio_transcription;
        self.audio_translation |= o.audio_translation;
        self.video |= o.video;
        self.video_input |= o.video_input;
        self.tools |= o.tools;
        self.vision |= o.vision;
        self.json_mode |= o.json_mode;
        self.streaming |= o.streaming;
        self.context_length = self.context_length.max(o.context_length);
        self.modalities.extend(o.modalities.iter().cloned());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_unprobed_only_for_text_generation() {
        assert!(ApiType::Chat.admits_unprobed());
        assert!(ApiType::Completions.admits_unprobed());
        assert!(ApiType::Multi.admits_unprobed());
        // Embeddings and media need probed/declared flags — an unprobed
        // deployment must never absorb them.
        for api in [
            ApiType::Embeddings,
            ApiType::Images,
            ApiType::AudioSpeech,
            ApiType::AudioTranscription,
            ApiType::AudioTranslation,
            ApiType::Video,
        ] {
            assert!(!api.admits_unprobed(), "{api} must not admit unprobed");
        }
    }

    #[test]
    fn supports_api_maps_each_type() {
        let c = Capabilities {
            embeddings: true,
            images: true,
            ..Default::default()
        };
        assert!(!c.supports_api(ApiType::Chat));
        assert!(c.supports_api(ApiType::Embeddings));
        assert!(c.supports_api(ApiType::Images));
        // Multi = "serves at least one family" (legacy/union semantics).
        assert!(c.supports_api(ApiType::Multi));
        assert!(!c.supports_api(ApiType::Video));
    }
}

#[cfg(test)]
mod union_tests {
    use super::*;

    #[test]
    fn union_ors_flags_max_ctx_and_merges_modalities() {
        let mut a = Capabilities {
            chat: true,
            context_length: 4096,
            modalities: ["text"].into_iter().map(String::from).collect(),
            ..Default::default()
        };
        let b = Capabilities {
            embeddings: true,
            vision: true,
            context_length: 8192,
            modalities: ["text", "image"].into_iter().map(String::from).collect(),
            ..Default::default()
        };
        a.union_with(&b);
        assert!(a.chat && a.embeddings && a.vision);
        assert_eq!(a.context_length, 8192);
        assert!(a.modalities.contains("image") && a.modalities.contains("text"));
        assert!(!a.images && !a.video);
    }
}
