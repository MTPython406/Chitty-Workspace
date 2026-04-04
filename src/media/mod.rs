//! Media generation framework — unified adaptor for image, video, and audio
//!
//! Provides a provider-agnostic `MediaAdaptor` trait that normalizes
//! image generation, video generation, TTS, and image editing across
//! all supported providers (xAI, OpenAI, Google, local sidecar).
//!
//! Core native tools in `src/tools/media.rs` use this framework.
//! Marketplace packages can build rich UIs (editors, composers, builders)
//! on top of these core capabilities.

pub mod xai;
pub mod openai;
pub mod google;
pub mod huggingface;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Capability flags
// ---------------------------------------------------------------------------

/// What media operations a provider supports
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaCapabilities {
    pub image_generation: bool,
    pub image_editing: bool,
    pub video_generation: bool,
    pub text_to_speech: bool,
    pub speech_to_text: bool,
    pub realtime_audio: bool,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRequest {
    pub prompt: String,
    /// Number of images to generate (1-4)
    pub n: u32,
    /// Aspect ratio: "1:1", "16:9", "9:16", "4:3", "3:4"
    pub aspect_ratio: String,
    /// Quality tier: "standard" or "pro"
    pub quality: String,
}

impl Default for ImageRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            n: 1,
            aspect_ratio: "1:1".to_string(),
            quality: "standard".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEditRequest {
    /// Edit instruction (e.g., "add snow to the mountains")
    pub prompt: String,
    /// Source image as base64-encoded data
    pub source_image_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoRequest {
    pub prompt: String,
    /// Duration in seconds (1-15 for most providers)
    pub duration: Option<u32>,
    /// Aspect ratio: "16:9", "9:16", "1:1"
    pub aspect_ratio: Option<String>,
    /// Optional image URL for image-to-video
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsRequest {
    pub text: String,
    /// Provider-specific voice ID (e.g., "eve" for xAI, "alloy" for OpenAI)
    pub voice: Option<String>,
    /// Speed multiplier (0.5-2.0)
    pub speed: Option<f32>,
    /// Output format: "mp3", "wav", "pcm"
    pub format: Option<String>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedImage {
    /// Raw image data as base64
    pub base64: String,
    /// Image format: "png", "jpg", "webp"
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
    pub model: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoResponse {
    /// Raw video bytes
    pub video_data: Vec<u8>,
    /// Video format: "mp4"
    pub format: String,
    /// Actual duration in seconds
    pub duration: f32,
    pub model: String,
    pub provider: String,
}

/// Speech-to-text request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttRequest {
    /// Base64-encoded audio data
    pub audio_base64: String,
    /// Whisper model ID (default: openai/whisper-large-v3-turbo)
    pub model: Option<String>,
    /// Language code (auto-detected if omitted)
    pub language: Option<String>,
    /// "transcribe" or "translate" (translate to English)
    pub task: String,
}

impl Default for SttRequest {
    fn default() -> Self {
        Self {
            audio_base64: String::new(),
            model: None,
            language: None,
            task: "transcribe".to_string(),
        }
    }
}

/// Speech-to-text response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttResponse {
    pub text: String,
    pub model: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioResponse {
    /// Raw audio bytes
    pub audio_data: Vec<u8>,
    /// Audio format: "mp3", "wav"
    pub format: String,
    /// Estimated duration in seconds
    pub duration_estimate: Option<f32>,
    pub provider: String,
}

// ---------------------------------------------------------------------------
// MediaAdaptor trait
// ---------------------------------------------------------------------------

/// Unified interface for media generation across all providers.
///
/// Each provider implements this trait, translating between Chitty's
/// standard request/response types and the provider's specific API.
#[async_trait]
pub trait MediaAdaptor: Send + Sync {
    /// Generate image(s) from a text prompt
    async fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse>;

    /// Edit an existing image with a text prompt
    async fn edit_image(&self, req: ImageEditRequest) -> Result<ImageResponse>;

    /// Generate a video from a text prompt (may involve polling for async APIs)
    async fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse>;

    /// Convert text to speech audio
    async fn text_to_speech(&self, req: TtsRequest) -> Result<AudioResponse>;

    /// Transcribe audio to text (speech-to-text)
    async fn speech_to_text(&self, req: SttRequest) -> Result<SttResponse>;

    /// What capabilities does this provider support?
    fn capabilities(&self) -> MediaCapabilities;

    /// Provider identifier
    fn provider_id(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a media adaptor for the given provider.
///
/// The adaptor handles all API translation between Chitty's standard
/// types and the provider's specific endpoints/formats.
pub fn create_media_adaptor(
    provider: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Box<dyn MediaAdaptor>> {
    match provider {
        "xai" => Ok(Box::new(xai::XaiMediaAdaptor::new(
            api_key.to_string(),
            base_url.map(|s| s.to_string()),
        ))),
        "openai" => Ok(Box::new(openai::OpenaiMediaAdaptor::new(
            api_key.to_string(),
            base_url.map(|s| s.to_string()),
        ))),
        "google" => Ok(Box::new(google::GoogleMediaAdaptor::new(
            api_key.to_string(),
            base_url.map(|s| s.to_string()),
        ))),
        "local" | "huggingface" => Ok(Box::new(huggingface::HuggingFaceMediaAdaptor::new())),
        _ => anyhow::bail!("No media adaptor for provider: {}", provider),
    }
}

/// List available voices for TTS by provider
pub fn list_voices(provider: &str) -> Vec<VoiceInfo> {
    match provider {
        "xai" => vec![
            VoiceInfo { id: "eve".into(), name: "Eve".into(), description: "Energetic and upbeat".into() },
            VoiceInfo { id: "ara".into(), name: "Ara".into(), description: "Warm and friendly".into() },
            VoiceInfo { id: "rex".into(), name: "Rex".into(), description: "Confident and professional".into() },
            VoiceInfo { id: "sal".into(), name: "Sal".into(), description: "Smooth and versatile".into() },
            VoiceInfo { id: "leo".into(), name: "Leo".into(), description: "Authoritative and strong".into() },
        ],
        "openai" => vec![
            VoiceInfo { id: "alloy".into(), name: "Alloy".into(), description: "Neutral and balanced".into() },
            VoiceInfo { id: "ash".into(), name: "Ash".into(), description: "Warm and engaging".into() },
            VoiceInfo { id: "coral".into(), name: "Coral".into(), description: "Clear and bright".into() },
            VoiceInfo { id: "echo".into(), name: "Echo".into(), description: "Resonant and deep".into() },
            VoiceInfo { id: "nova".into(), name: "Nova".into(), description: "Friendly and expressive".into() },
            VoiceInfo { id: "onyx".into(), name: "Onyx".into(), description: "Rich and authoritative".into() },
            VoiceInfo { id: "sage".into(), name: "Sage".into(), description: "Calm and measured".into() },
            VoiceInfo { id: "shimmer".into(), name: "Shimmer".into(), description: "Light and melodic".into() },
            VoiceInfo { id: "marin".into(), name: "Marin".into(), description: "Natural and conversational".into() },
            VoiceInfo { id: "cedar".into(), name: "Cedar".into(), description: "Warm and mature".into() },
        ],
        _ => vec![],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}
