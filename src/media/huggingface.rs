//! HuggingFace Media Adaptor — local diffusion models via sidecar
//!
//! Supports image generation through Stable Diffusion and similar models
//! running via the HuggingFace Python sidecar. TTS possible via Bark/XTTS.
//! Requires model download and GPU recommended.
//!
//! Note: This is a stub — full implementation requires the Python sidecar
//! to support image generation endpoints.

use anyhow::Result;
use async_trait::async_trait;

use super::{
    AudioResponse, ImageEditRequest, ImageRequest, ImageResponse,
    MediaAdaptor, MediaCapabilities, TtsRequest, VideoRequest, VideoResponse,
};

pub struct HuggingFaceMediaAdaptor;

impl HuggingFaceMediaAdaptor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MediaAdaptor for HuggingFaceMediaAdaptor {
    fn provider_id(&self) -> &str {
        "huggingface"
    }

    fn capabilities(&self) -> MediaCapabilities {
        // Will be enabled as sidecar support is added
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
            "HuggingFace image generation is not yet implemented. \
             This feature requires a local Stable Diffusion model via the Python sidecar. \
             Use a cloud provider (xAI, OpenAI, or Google) for now."
        )
    }

    async fn edit_image(&self, _req: ImageEditRequest) -> Result<ImageResponse> {
        anyhow::bail!("HuggingFace image editing is not yet implemented.")
    }

    async fn generate_video(&self, _req: VideoRequest) -> Result<VideoResponse> {
        anyhow::bail!("HuggingFace video generation is not yet implemented.")
    }

    async fn text_to_speech(&self, _req: TtsRequest) -> Result<AudioResponse> {
        anyhow::bail!(
            "HuggingFace TTS is not yet implemented. \
             This feature will support Bark/XTTS models via the Python sidecar."
        )
    }
}
