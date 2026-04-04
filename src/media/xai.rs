//! xAI Media Adaptor — Image (grok-imagine), Video, TTS
//!
//! Endpoints:
//! - Image Gen:   POST https://api.x.ai/v1/images/generations
//! - Image Edit:  POST https://api.x.ai/v1/images/edits
//! - Video Gen:   POST https://api.x.ai/v1/videos/generations (async + polling)
//! - Video Poll:  GET  https://api.x.ai/v1/videos/{request_id}
//! - TTS:         POST https://api.x.ai/v1/tts

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::{
    AudioResponse, GeneratedImage, ImageEditRequest, ImageRequest, ImageResponse,
    MediaAdaptor, MediaCapabilities, SttRequest, SttResponse, TtsRequest, VideoRequest, VideoResponse,
};

pub struct XaiMediaAdaptor {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl XaiMediaAdaptor {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.x.ai/v1".to_string()),
        }
    }
}

#[async_trait]
impl MediaAdaptor for XaiMediaAdaptor {
    fn provider_id(&self) -> &str {
        "xai"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            image_generation: true,
            image_editing: true,
            video_generation: true,
            text_to_speech: true,
            speech_to_text: false,
            realtime_audio: true,
        }
    }

    async fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse> {
        let model = if req.quality == "pro" {
            "grok-imagine-image-pro"
        } else {
            "grok-imagine-image"
        };

        info!("xAI image generation: model={}, prompt_len={}", model, req.prompt.len());

        let body = serde_json::json!({
            "model": model,
            "prompt": req.prompt,
            "n": req.n.min(4),
            "response_format": "b64_json",
            "aspect_ratio": req.aspect_ratio,
        });

        let resp = self.client
            .post(format!("{}/images/generations", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call xAI image generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("xAI image generation failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await
            .context("Failed to parse xAI image response")?;

        let data = json["data"].as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing 'data' array in xAI image response"))?;

        let images: Vec<GeneratedImage> = data.iter().filter_map(|item| {
            let b64 = item["b64_json"].as_str()?;
            Some(GeneratedImage {
                base64: b64.to_string(),
                format: "png".to_string(),
            })
        }).collect();

        if images.is_empty() {
            anyhow::bail!("xAI returned no images");
        }

        debug!("xAI generated {} image(s)", images.len());

        Ok(ImageResponse {
            images,
            model: model.to_string(),
            provider: "xai".to_string(),
        })
    }

    async fn edit_image(&self, req: ImageEditRequest) -> Result<ImageResponse> {
        info!("xAI image edit: prompt_len={}", req.prompt.len());

        // xAI expects image_url as a data URI for base64
        let image_url = if req.source_image_base64.starts_with("data:") {
            req.source_image_base64.clone()
        } else {
            format!("data:image/png;base64,{}", req.source_image_base64)
        };

        let body = serde_json::json!({
            "model": "grok-imagine-image",
            "prompt": req.prompt,
            "image_url": image_url,
            "response_format": "b64_json",
        });

        let resp = self.client
            .post(format!("{}/images/edits", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call xAI image edit API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("xAI image edit failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;
        let data = json["data"].as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing 'data' in xAI edit response"))?;

        let images: Vec<GeneratedImage> = data.iter().filter_map(|item| {
            let b64 = item["b64_json"].as_str()?;
            Some(GeneratedImage {
                base64: b64.to_string(),
                format: "png".to_string(),
            })
        }).collect();

        Ok(ImageResponse {
            images,
            model: "grok-imagine-image".to_string(),
            provider: "xai".to_string(),
        })
    }

    async fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse> {
        info!("xAI video generation: prompt_len={}, duration={:?}", req.prompt.len(), req.duration);

        let mut body = serde_json::json!({
            "model": "grok-imagine-video",
            "prompt": req.prompt,
        });

        if let Some(dur) = req.duration {
            body["duration"] = serde_json::json!(dur.min(15));
        }
        if let Some(ref ar) = req.aspect_ratio {
            body["aspect_ratio"] = serde_json::json!(ar);
        }
        if let Some(ref img) = req.image_url {
            body["image_url"] = serde_json::json!(img);
        }

        // Step 1: Submit video generation request
        let resp = self.client
            .post(format!("{}/videos/generations", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call xAI video generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("xAI video generation failed ({}): {}", status, err_body);
        }

        let submit_json: serde_json::Value = resp.json().await?;
        let request_id = submit_json["request_id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing request_id in xAI video response"))?
            .to_string();

        info!("xAI video submitted, request_id={}", request_id);

        // Step 2: Poll for completion (max 2 minutes, every 5 seconds)
        let max_polls = 24; // 24 * 5s = 120s
        for i in 0..max_polls {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            let poll_resp = self.client
                .get(format!("{}/videos/{}", self.base_url, request_id))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .send()
                .await?;

            let poll_json: serde_json::Value = poll_resp.json().await?;
            let poll_status = poll_json["status"].as_str().unwrap_or("unknown");

            debug!("xAI video poll {}/{}: status={}", i + 1, max_polls, poll_status);

            match poll_status {
                "done" | "DONE" => {
                    // Download the video
                    let video_url = poll_json["video"]["url"].as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing video URL in poll response"))?;

                    let duration = poll_json["video"]["duration"].as_f64().unwrap_or(5.0) as f32;

                    let video_bytes = self.client
                        .get(video_url)
                        .send()
                        .await?
                        .bytes()
                        .await?
                        .to_vec();

                    info!("xAI video downloaded: {} bytes, {:.1}s", video_bytes.len(), duration);

                    return Ok(VideoResponse {
                        video_data: video_bytes,
                        format: "mp4".to_string(),
                        duration,
                        model: "grok-imagine-video".to_string(),
                        provider: "xai".to_string(),
                    });
                }
                "failed" | "FAILED" => {
                    let reason = poll_json["error"].as_str().unwrap_or("Unknown error");
                    anyhow::bail!("xAI video generation failed: {}", reason);
                }
                "expired" | "EXPIRED" => {
                    anyhow::bail!("xAI video generation expired before completion");
                }
                _ => {
                    // Still pending, continue polling
                }
            }
        }

        anyhow::bail!("xAI video generation timed out after 2 minutes")
    }

    async fn text_to_speech(&self, req: TtsRequest) -> Result<AudioResponse> {
        let voice = req.voice.as_deref().unwrap_or("eve");
        let format = req.format.as_deref().unwrap_or("mp3");

        info!("xAI TTS: voice={}, text_len={}", voice, req.text.len());

        // xAI TTS endpoint
        let body = serde_json::json!({
            "text": req.text,
            "voice": voice,
            "language": "en",
        });

        let resp = self.client
            .post(format!("{}/tts", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call xAI TTS API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("xAI TTS failed ({}): {}", status, err_body);
        }

        let audio_bytes = resp.bytes().await?.to_vec();

        // Rough estimate: ~150 words per minute, ~5 chars per word
        let char_count = req.text.len() as f32;
        let duration_estimate = (char_count / 5.0) / 150.0 * 60.0;

        info!("xAI TTS complete: {} bytes", audio_bytes.len());

        Ok(AudioResponse {
            audio_data: audio_bytes,
            format: format.to_string(),
            duration_estimate: Some(duration_estimate),
            provider: "xai".to_string(),
        })
    }

    async fn speech_to_text(&self, _req: SttRequest) -> Result<SttResponse> {
        anyhow::bail!("xAI does not support speech-to-text. Use a local Whisper model or OpenAI/Google.")
    }
}
