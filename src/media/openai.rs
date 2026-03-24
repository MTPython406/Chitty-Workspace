//! OpenAI Media Adaptor — GPT-Image, Sora, TTS
//!
//! Endpoints:
//! - Image Gen:   POST https://api.openai.com/v1/images/generations  (gpt-image-1.5)
//! - Image Edit:  POST https://api.openai.com/v1/images/edits
//! - Video Gen:   POST https://api.openai.com/v1/videos/generations  (sora-2)
//! - TTS:         POST https://api.openai.com/v1/audio/speech        (gpt-4o-mini-tts)

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::{
    AudioResponse, GeneratedImage, ImageEditRequest, ImageRequest, ImageResponse,
    MediaAdaptor, MediaCapabilities, TtsRequest, VideoRequest, VideoResponse,
};

pub struct OpenaiMediaAdaptor {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenaiMediaAdaptor {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        }
    }
}

#[async_trait]
impl MediaAdaptor for OpenaiMediaAdaptor {
    fn provider_id(&self) -> &str {
        "openai"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            image_generation: true,
            image_editing: true,
            video_generation: true,
            text_to_speech: true,
            speech_to_text: true,
            realtime_audio: true,
        }
    }

    async fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse> {
        let model = if req.quality == "pro" {
            "gpt-image-1"
        } else {
            "gpt-image-1.5"
        };

        info!("OpenAI image generation: model={}, prompt_len={}", model, req.prompt.len());

        // Map aspect ratio to OpenAI size format
        let size = match req.aspect_ratio.as_str() {
            "16:9" => "1792x1024",
            "9:16" => "1024x1792",
            _ => "1024x1024", // 1:1 and others
        };

        let body = serde_json::json!({
            "model": model,
            "prompt": req.prompt,
            "n": req.n.min(4),
            "size": size,
            "response_format": "b64_json",
        });

        let resp = self.client
            .post(format!("{}/images/generations", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call OpenAI image generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI image generation failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;
        let data = json["data"].as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing 'data' in OpenAI image response"))?;

        let images: Vec<GeneratedImage> = data.iter().filter_map(|item| {
            let b64 = item["b64_json"].as_str()?;
            Some(GeneratedImage {
                base64: b64.to_string(),
                format: "png".to_string(),
            })
        }).collect();

        debug!("OpenAI generated {} image(s)", images.len());

        Ok(ImageResponse {
            images,
            model: model.to_string(),
            provider: "openai".to_string(),
        })
    }

    async fn edit_image(&self, req: ImageEditRequest) -> Result<ImageResponse> {
        info!("OpenAI image edit: prompt_len={}", req.prompt.len());

        // OpenAI edit API accepts base64 via the image field
        let body = serde_json::json!({
            "model": "gpt-image-1.5",
            "prompt": req.prompt,
            "image": [{
                "type": "base64",
                "media_type": "image/png",
                "data": req.source_image_base64,
            }],
            "response_format": "b64_json",
        });

        let resp = self.client
            .post(format!("{}/images/edits", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call OpenAI image edit API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI image edit failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;
        let data = json["data"].as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing 'data' in OpenAI edit response"))?;

        let images: Vec<GeneratedImage> = data.iter().filter_map(|item| {
            let b64 = item["b64_json"].as_str()?;
            Some(GeneratedImage {
                base64: b64.to_string(),
                format: "png".to_string(),
            })
        }).collect();

        Ok(ImageResponse {
            images,
            model: "gpt-image-1.5".to_string(),
            provider: "openai".to_string(),
        })
    }

    async fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse> {
        info!("OpenAI video generation: prompt_len={}", req.prompt.len());

        let mut body = serde_json::json!({
            "model": "sora-2",
            "prompt": req.prompt,
        });

        if let Some(dur) = req.duration {
            body["duration"] = serde_json::json!(dur);
        }
        if let Some(ref ar) = req.aspect_ratio {
            body["aspect_ratio"] = serde_json::json!(ar);
        }

        // OpenAI Sora API — submit and poll (similar to xAI)
        let resp = self.client
            .post(format!("{}/videos/generations", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call OpenAI video generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI video generation failed ({}): {}", status, err_body);
        }

        let submit_json: serde_json::Value = resp.json().await?;

        // Check if response is synchronous (has video data) or async (has id for polling)
        if let Some(video_url) = submit_json["data"][0]["url"].as_str() {
            // Synchronous response
            let video_bytes = self.client.get(video_url).send().await?.bytes().await?.to_vec();
            let duration = submit_json["data"][0]["duration"].as_f64().unwrap_or(5.0) as f32;

            return Ok(VideoResponse {
                video_data: video_bytes,
                format: "mp4".to_string(),
                duration,
                model: "sora-2".to_string(),
                provider: "openai".to_string(),
            });
        }

        // Async response — poll for completion
        let request_id = submit_json["id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing id in OpenAI video response"))?
            .to_string();

        let max_polls = 36; // 36 * 5s = 3 minutes
        for i in 0..max_polls {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            let poll_resp = self.client
                .get(format!("{}/videos/generations/{}", self.base_url, request_id))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .send()
                .await?;

            let poll_json: serde_json::Value = poll_resp.json().await?;
            let poll_status = poll_json["status"].as_str().unwrap_or("unknown");

            debug!("OpenAI video poll {}/{}: status={}", i + 1, max_polls, poll_status);

            if poll_status == "completed" || poll_status == "succeeded" {
                let video_url = poll_json["data"][0]["url"].as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing video URL"))?;
                let duration = poll_json["data"][0]["duration"].as_f64().unwrap_or(5.0) as f32;
                let video_bytes = self.client.get(video_url).send().await?.bytes().await?.to_vec();

                return Ok(VideoResponse {
                    video_data: video_bytes,
                    format: "mp4".to_string(),
                    duration,
                    model: "sora-2".to_string(),
                    provider: "openai".to_string(),
                });
            }

            if poll_status == "failed" {
                anyhow::bail!("OpenAI video generation failed");
            }
        }

        anyhow::bail!("OpenAI video generation timed out after 3 minutes")
    }

    async fn text_to_speech(&self, req: TtsRequest) -> Result<AudioResponse> {
        let voice = req.voice.as_deref().unwrap_or("alloy");
        let format = req.format.as_deref().unwrap_or("mp3");
        let speed = req.speed.unwrap_or(1.0);

        info!("OpenAI TTS: voice={}, text_len={}", voice, req.text.len());

        let body = serde_json::json!({
            "model": "gpt-4o-mini-tts",
            "input": req.text,
            "voice": voice,
            "response_format": format,
            "speed": speed,
        });

        let resp = self.client
            .post(format!("{}/audio/speech", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call OpenAI TTS API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI TTS failed ({}): {}", status, err_body);
        }

        let audio_bytes = resp.bytes().await?.to_vec();
        let char_count = req.text.len() as f32;
        let duration_estimate = (char_count / 5.0) / 150.0 * 60.0 / speed;

        info!("OpenAI TTS complete: {} bytes", audio_bytes.len());

        Ok(AudioResponse {
            audio_data: audio_bytes,
            format: format.to_string(),
            duration_estimate: Some(duration_estimate),
            provider: "openai".to_string(),
        })
    }
}
