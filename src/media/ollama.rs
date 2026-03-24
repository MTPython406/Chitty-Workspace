//! Ollama Media Adaptor — limited media support for local models
//!
//! Ollama primarily supports text chat. Image generation may be available
//! through Stable Diffusion models if configured, but is not standard.
//! TTS and video are not supported.

use anyhow::Result;
use async_trait::async_trait;

use super::{
    AudioResponse, ImageEditRequest, ImageRequest, ImageResponse,
    MediaAdaptor, MediaCapabilities, TtsRequest, VideoRequest, VideoResponse,
};

pub struct OllamaMediaAdaptor {
    _base_url: String,
}

impl OllamaMediaAdaptor {
    pub fn new(base_url: Option<String>) -> Self {
        Self {
            _base_url: base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
        }
    }
}

#[async_trait]
impl MediaAdaptor for OllamaMediaAdaptor {
    fn provider_id(&self) -> &str {
        "ollama"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            image_generation: false,
            image_editing: false,
            video_generation: false,
            text_to_speech: false,
            speech_to_text: false,
            realtime_audio: false,
        }
    }

    async fn generate_image(&self, _req: ImageRequest) -> Result<ImageResponse> {
        anyhow::bail!(
            "Ollama does not support image generation. \
             Use a cloud provider (xAI, OpenAI, or Google) for image generation, \
             or install a HuggingFace diffusion model."
        )
    }

    async fn edit_image(&self, _req: ImageEditRequest) -> Result<ImageResponse> {
        anyhow::bail!("Ollama does not support image editing.")
    }

    async fn generate_video(&self, _req: VideoRequest) -> Result<VideoResponse> {
        anyhow::bail!("Ollama does not support video generation.")
    }

    async fn text_to_speech(&self, _req: TtsRequest) -> Result<AudioResponse> {
        anyhow::bail!("Ollama does not support text-to-speech.")
    }
}
