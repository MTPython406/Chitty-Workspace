//! HuggingFace Media Adaptor — local image, video, and TTS via sidecar
//!
//! Supports image generation through diffusers models (Flux, SDXL, SD3, etc.),
//! video generation (CogVideoX, Wan, LTX-Video), and text-to-speech
//! (Bark, SpeechT5, Parler) running via the Python sidecar.
//!
//! Models must be registered and loaded via the sidecar /media/models/* endpoints.
//! Only one model can occupy GPU VRAM at a time.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use super::{
    AudioResponse, ImageEditRequest, ImageRequest, ImageResponse, GeneratedImage,
    MediaAdaptor, MediaCapabilities, SttRequest, SttResponse, TtsRequest, VideoRequest, VideoResponse,
};

pub struct HuggingFaceMediaAdaptor {
    base_url: String,
}

impl HuggingFaceMediaAdaptor {
    pub fn new() -> Self {
        // Read port from config; fallback to default 8766
        let port = match crate::storage::default_data_dir() {
            data_dir => {
                crate::config::AppConfig::load(&data_dir)
                    .map(|c| c.local.sidecar_port)
                    .unwrap_or(8766)
            }
        };
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }
}

#[async_trait]
impl MediaAdaptor for HuggingFaceMediaAdaptor {
    fn provider_id(&self) -> &str {
        "huggingface"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            image_generation: true,
            image_editing: false, // Not yet — needs img2img pipeline
            video_generation: true,
            text_to_speech: true,
            speech_to_text: true,
            realtime_audio: false,
        }
    }

    async fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse> {
        // Map quality tier to inference steps
        let steps: u32 = match req.quality.as_str() {
            "pro" => 50,
            _ => 25, // "standard"
        };

        let result = crate::huggingface::generate_image_local(
            &self.base_url,
            &req.prompt,
            req.n,
            &req.aspect_ratio,
            steps,
            7.5, // default guidance_scale
            None, // random seed
        )
        .await
        .context("Local image generation failed")?;

        // Parse response: { images: [{ base64, format }], model, provider }
        let images_arr = result
            .get("images")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("No images in sidecar response"))?;

        let mut images = Vec::new();
        for img_val in images_arr {
            let b64 = img_val
                .get("base64")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let format = img_val
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("png")
                .to_string();
            images.push(GeneratedImage {
                base64: b64,
                format,
            });
        }

        let model = result
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        Ok(ImageResponse {
            images,
            model,
            provider: "huggingface".to_string(),
        })
    }

    async fn edit_image(&self, _req: ImageEditRequest) -> Result<ImageResponse> {
        anyhow::bail!(
            "Image editing is not yet supported for local models. \
             Use a cloud provider (xAI, OpenAI, or Google) for image editing."
        )
    }

    async fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse> {
        // Map duration to number of frames (default 8 fps)
        let fps = 8u32;
        let num_frames = req.duration.map(|d| d * fps).unwrap_or(49);

        let aspect_ratio = req.aspect_ratio.as_deref().unwrap_or("16:9");

        let result = crate::huggingface::generate_video_local(
            &self.base_url,
            &req.prompt,
            num_frames,
            aspect_ratio,
            50,  // default steps for video
            6.0, // default guidance_scale for video
            None,
        )
        .await
        .context("Local video generation failed")?;

        // Parse response: { video_base64, format, duration, model, provider }
        let video_b64 = result
            .get("video_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No video_base64 in sidecar response"))?;

        let video_data = BASE64
            .decode(video_b64)
            .context("Failed to decode video base64")?;

        let duration = result
            .get("duration")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32;

        let model = result
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        Ok(VideoResponse {
            video_data,
            format: "mp4".to_string(),
            duration,
            model,
            provider: "huggingface".to_string(),
        })
    }

    async fn text_to_speech(&self, req: TtsRequest) -> Result<AudioResponse> {
        let speed = req.speed.unwrap_or(1.0);
        let format = req.format.as_deref().unwrap_or("wav");

        let result = crate::huggingface::text_to_speech_local(
            &self.base_url,
            &req.text,
            req.voice.as_deref(),
            speed,
            format,
        )
        .await
        .context("Local TTS generation failed")?;

        // Parse response: { audio_base64, format, duration_estimate, model, provider }
        let audio_b64 = result
            .get("audio_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No audio_base64 in sidecar response"))?;

        let audio_data = BASE64
            .decode(audio_b64)
            .context("Failed to decode audio base64")?;

        let duration_estimate = result
            .get("duration_estimate")
            .and_then(|v| v.as_f64())
            .map(|d| d as f32);

        Ok(AudioResponse {
            audio_data,
            format: format.to_string(),
            duration_estimate,
            provider: "huggingface".to_string(),
        })
    }

    async fn speech_to_text(&self, req: SttRequest) -> Result<SttResponse> {
        let result = crate::huggingface::speech_to_text_local(
            &self.base_url,
            &req.audio_base64,
            req.model.as_deref(),
            req.language.as_deref(),
            &req.task,
        )
        .await
        .context("Local speech-to-text failed")?;

        let text = result
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let model = result
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("whisper")
            .to_string();

        Ok(SttResponse {
            text,
            model,
            provider: "huggingface".to_string(),
        })
    }
}
