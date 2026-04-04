//! Google Gemini Media Adaptor — Imagen 4, Veo 3.1, Gemini TTS
//!
//! All endpoints use the Gemini API at generativelanguage.googleapis.com
//! Authentication: x-goog-api-key header

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::{
    AudioResponse, GeneratedImage, ImageEditRequest, ImageRequest, ImageResponse,
    MediaAdaptor, MediaCapabilities, SttRequest, SttResponse, TtsRequest, VideoRequest, VideoResponse,
};

pub struct GoogleMediaAdaptor {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl GoogleMediaAdaptor {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key,
            base_url: base_url.unwrap_or_else(|| {
                "https://generativelanguage.googleapis.com/v1beta".to_string()
            }),
        }
    }
}

#[async_trait]
impl MediaAdaptor for GoogleMediaAdaptor {
    fn provider_id(&self) -> &str {
        "google"
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
            "imagen-4-ultra"
        } else {
            "imagen-4"
        };

        info!("Google image generation: model={}, prompt_len={}", model, req.prompt.len());

        let body = serde_json::json!({
            "contents": [{
                "parts": [{
                    "text": req.prompt
                }]
            }],
            "generationConfig": {
                "responseModalities": ["IMAGE"],
                "imagenConfig": {
                    "numberOfImages": req.n.min(4),
                    "aspectRatio": req.aspect_ratio,
                }
            }
        });

        let resp = self.client
            .post(format!("{}/models/{}:generateContent", self.base_url, model))
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Google image generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google image generation failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;

        // Google returns images in candidates[0].content.parts[].inlineData
        let mut images = Vec::new();
        if let Some(candidates) = json["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(b64) = inline["data"].as_str() {
                                let mime = inline["mimeType"].as_str().unwrap_or("image/png");
                                let format = if mime.contains("jpeg") || mime.contains("jpg") {
                                    "jpg"
                                } else {
                                    "png"
                                };
                                images.push(GeneratedImage {
                                    base64: b64.to_string(),
                                    format: format.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        if images.is_empty() {
            anyhow::bail!("Google returned no images");
        }

        debug!("Google generated {} image(s)", images.len());

        Ok(ImageResponse {
            images,
            model: model.to_string(),
            provider: "google".to_string(),
        })
    }

    async fn edit_image(&self, req: ImageEditRequest) -> Result<ImageResponse> {
        info!("Google image edit: prompt_len={}", req.prompt.len());

        // Google uses the same generateContent endpoint with image input
        let body = serde_json::json!({
            "contents": [{
                "parts": [
                    { "text": req.prompt },
                    {
                        "inlineData": {
                            "mimeType": "image/png",
                            "data": req.source_image_base64
                        }
                    }
                ]
            }],
            "generationConfig": {
                "responseModalities": ["IMAGE"],
            }
        });

        let resp = self.client
            .post(format!("{}/models/imagen-4:generateContent", self.base_url))
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Google image edit API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google image edit failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;
        let mut images = Vec::new();
        if let Some(candidates) = json["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(b64) = inline["data"].as_str() {
                                images.push(GeneratedImage {
                                    base64: b64.to_string(),
                                    format: "png".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(ImageResponse {
            images,
            model: "imagen-4".to_string(),
            provider: "google".to_string(),
        })
    }

    async fn generate_video(&self, req: VideoRequest) -> Result<VideoResponse> {
        info!("Google video generation: prompt_len={}", req.prompt.len());

        let body = serde_json::json!({
            "contents": [{
                "parts": [{
                    "text": req.prompt
                }]
            }],
            "generationConfig": {
                "responseModalities": ["VIDEO"],
            }
        });

        let resp = self.client
            .post(format!("{}/models/veo-3.1-generate-preview:generateContent", self.base_url))
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Google video generation API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google video generation failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;

        // Extract video data from response
        if let Some(candidates) = json["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(b64) = inline["data"].as_str() {
                                let video_data = base64::Engine::decode(
                                    &base64::engine::general_purpose::STANDARD,
                                    b64,
                                )?;
                                return Ok(VideoResponse {
                                    video_data,
                                    format: "mp4".to_string(),
                                    duration: req.duration.unwrap_or(8) as f32,
                                    model: "veo-3.1".to_string(),
                                    provider: "google".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        anyhow::bail!("Google returned no video data")
    }

    async fn text_to_speech(&self, req: TtsRequest) -> Result<AudioResponse> {
        info!("Google TTS: text_len={}", req.text.len());

        let body = serde_json::json!({
            "contents": [{
                "parts": [{
                    "text": req.text
                }]
            }],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": {
                        "prebuiltVoiceConfig": {
                            "voiceName": req.voice.as_deref().unwrap_or("Kore")
                        }
                    }
                }
            }
        });

        let resp = self.client
            .post(format!("{}/models/gemini-2.5-flash:generateContent", self.base_url))
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Google TTS API")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google TTS failed ({}): {}", status, err_body);
        }

        let json: serde_json::Value = resp.json().await?;

        // Extract audio from response
        if let Some(candidates) = json["candidates"].as_array() {
            for candidate in candidates {
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(b64) = inline["data"].as_str() {
                                let audio_data = base64::Engine::decode(
                                    &base64::engine::general_purpose::STANDARD,
                                    b64,
                                )?;
                                let char_count = req.text.len() as f32;
                                let duration_estimate = (char_count / 5.0) / 150.0 * 60.0;

                                return Ok(AudioResponse {
                                    audio_data,
                                    format: "mp3".to_string(),
                                    duration_estimate: Some(duration_estimate),
                                    provider: "google".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        anyhow::bail!("Google returned no audio data")
    }

    async fn speech_to_text(&self, _req: SttRequest) -> Result<SttResponse> {
        // TODO: Implement via Google Cloud Speech-to-Text API
        anyhow::bail!("Google speech-to-text not yet implemented. Use a local Whisper model instead.")
    }
}
