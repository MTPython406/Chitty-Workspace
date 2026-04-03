//! Media generation tools — native tools for image, video, audio, and editing
//!
//! These are core Layer 1 tools. Marketplace packages (Layer 2) build rich
//! UIs on top of these — image editors, video builders, social composers.

use async_trait::async_trait;
use std::path::PathBuf;
use tracing::{info, warn};

use super::{NativeTool, ToolCategory, ToolContext, ToolDefinition, ToolResult};
use crate::media;
use crate::storage;

/// Get the media directory, creating it if needed
fn media_dir(subdir: &str) -> PathBuf {
    let dir = storage::default_data_dir().join("media").join(subdir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Generate a unique filename with timestamp
fn unique_filename(prefix: &str, ext: &str) -> String {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let hash = &uuid::Uuid::new_v4().to_string()[..8];
    format!("{}{}_{}.{}", prefix, ts, hash, ext)
}

/// Get the provider and API key for media operations.
/// Resolution order: 1) explicit arg, 2) system default for capability, 3) first provider with key
fn get_provider_config(args: &serde_json::Value, capability: &str) -> Result<(String, String), String> {
    // 1. Check if provider is explicitly specified in tool args
    if let Some(p) = args["provider"].as_str() {
        // Local providers don't need API keys
        if p == "huggingface" {
            return Ok(("huggingface".to_string(), String::new()));
        }
        if let Ok(Some(key)) = crate::config::get_api_key(p) {
            return Ok((p.to_string(), key));
        }
        return Err(format!("No API key configured for '{}'. Add one in Settings > Providers.", p));
    }

    // 2. Check system defaults for this capability
    let data_dir = storage::default_data_dir();
    if let Ok(config) = crate::config::AppConfig::load(&data_dir) {
        let default_provider = match capability {
            "image" => config.defaults.image_provider.as_deref(),
            "video" => config.defaults.video_provider.as_deref(),
            "tts" => config.defaults.tts_provider.as_deref(),
            "stt" => config.defaults.stt_provider.as_deref(),
            _ => None,
        };
        if let Some(p) = default_provider {
            // Local providers don't need API keys
            if p == "huggingface" {
                info!("Using system default local provider 'huggingface' for {}", capability);
                return Ok(("huggingface".to_string(), String::new()));
            }
            if let Ok(Some(key)) = crate::config::get_api_key(p) {
                info!("Using system default provider '{}' for {}", p, capability);
                return Ok((p.to_string(), key));
            }
        }
    }

    // 3. Fallback: find first provider with an API key
    for provider in &["xai", "openai", "google"] {
        if let Ok(Some(key)) = crate::config::get_api_key(provider) {
            info!("Auto-selected provider '{}' for {} (no default set)", provider, capability);
            return Ok((provider.to_string(), key));
        }
    }

    Err(format!("No provider configured for {}. Add an API key in Settings > Providers, or set up a local model via huggingface.", capability))
}

// ─── Generate Image ─────────────────────────────────────────────────────────

pub struct GenerateImageTool;

#[async_trait]
impl NativeTool for GenerateImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_image".to_string(),
            display_name: "Generate Image".to_string(),
            description: "Generate image(s) from a text prompt using AI. Saves to the media folder and returns the image for display.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Detailed description of the image to generate"
                    },
                    "aspect_ratio": {
                        "type": "string",
                        "enum": ["1:1", "16:9", "9:16", "4:3", "3:4"],
                        "description": "Image aspect ratio (default: 1:1)"
                    },
                    "quality": {
                        "type": "string",
                        "enum": ["standard", "pro"],
                        "description": "Quality tier — 'pro' uses higher quality model (default: standard)"
                    },
                    "n": {
                        "type": "integer",
                        "description": "Number of images to generate (1-4, default: 1)",
                        "minimum": 1,
                        "maximum": 4
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["xai", "openai", "google", "huggingface"],
                        "description": "Provider to use for generation (default: xai)"
                    }
                },
                "required": ["prompt"]
            }),
            instructions: Some(
                "Use generate_image when the user asks to create, generate, make, or draw an image, \
                 picture, illustration, photo, graphic, or visual. Pass a detailed prompt describing \
                 exactly what to generate. Choose aspect_ratio based on context (16:9 for banners, \
                 9:16 for phone wallpapers, 1:1 for social media). Use 'pro' quality when the user \
                 asks for high quality, detailed, or professional images."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let prompt = match args["prompt"].as_str() {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => return ToolResult::err("Missing required parameter: prompt"),
        };

        let (provider, api_key) = match get_provider_config(args, "image") {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let adaptor = match media::create_media_adaptor(&provider, &api_key, None) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("Failed to create media adaptor: {}", e)),
        };

        let req = media::ImageRequest {
            prompt: prompt.clone(),
            n: args["n"].as_u64().unwrap_or(1) as u32,
            aspect_ratio: args["aspect_ratio"].as_str().unwrap_or("1:1").to_string(),
            quality: args["quality"].as_str().unwrap_or("standard").to_string(),
        };

        match adaptor.generate_image(req).await {
            Ok(resp) => {
                let dir = media_dir("images");
                let mut saved_images = Vec::new();

                for (i, img) in resp.images.iter().enumerate() {
                    let filename = unique_filename("img_", &img.format);
                    let filepath = dir.join(&filename);

                    // Decode base64 and save to disk
                    match base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        &img.base64,
                    ) {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(&filepath, &bytes) {
                                warn!("Failed to save image to disk: {}", e);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to decode image base64: {}", e);
                        }
                    }

                    saved_images.push(serde_json::json!({
                        "filename": filename,
                        "path": filepath.to_string_lossy(),
                        "base64": img.base64,
                        "format": img.format,
                        "index": i,
                    }));
                }

                info!("Generated {} image(s) via {}", saved_images.len(), provider);

                ToolResult::ok(serde_json::json!({
                    "success": true,
                    "images": saved_images,
                    "prompt": prompt,
                    "model": resp.model,
                    "provider": resp.provider,
                    "count": saved_images.len(),
                }))
            }
            Err(e) => ToolResult::err(format!("Image generation failed: {}", e)),
        }
    }
}

// ─── Edit Image ─────────────────────────────────────────────────────────────

pub struct EditImageTool;

#[async_trait]
impl NativeTool for EditImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_image".to_string(),
            display_name: "Edit Image".to_string(),
            description: "Edit an existing image using a text prompt. Reads the source image from the media folder.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Description of the edit to make (e.g., 'add snow to the mountains')"
                    },
                    "image_filename": {
                        "type": "string",
                        "description": "Filename of the source image in the media/images/ folder"
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["xai", "openai", "google", "huggingface"],
                        "description": "Provider to use (default: xai)"
                    }
                },
                "required": ["prompt", "image_filename"]
            }),
            instructions: Some(
                "Use edit_image when the user wants to modify, change, update, or edit an existing \
                 generated image. Requires the image_filename from a previous generate_image result."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let prompt = match args["prompt"].as_str() {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => return ToolResult::err("Missing required parameter: prompt"),
        };

        let image_filename = match args["image_filename"].as_str() {
            Some(f) => f,
            None => return ToolResult::err("Missing required parameter: image_filename"),
        };

        // Read source image from media folder
        let source_path = media_dir("images").join(image_filename);
        let image_bytes = match std::fs::read(&source_path) {
            Ok(bytes) => bytes,
            Err(e) => return ToolResult::err(format!("Failed to read source image '{}': {}", image_filename, e)),
        };

        let source_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &image_bytes,
        );

        let (provider, api_key) = match get_provider_config(args, "image") {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let adaptor = match media::create_media_adaptor(&provider, &api_key, None) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("Failed to create media adaptor: {}", e)),
        };

        let req = media::ImageEditRequest {
            prompt: prompt.clone(),
            source_image_base64: source_b64,
        };

        match adaptor.edit_image(req).await {
            Ok(resp) => {
                let dir = media_dir("images");
                let mut saved_images = Vec::new();

                for img in &resp.images {
                    let filename = unique_filename("edit_", &img.format);
                    let filepath = dir.join(&filename);

                    if let Ok(bytes) = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        &img.base64,
                    ) {
                        let _ = std::fs::write(&filepath, &bytes);
                    }

                    saved_images.push(serde_json::json!({
                        "filename": filename,
                        "path": filepath.to_string_lossy(),
                        "base64": img.base64,
                        "format": img.format,
                    }));
                }

                ToolResult::ok(serde_json::json!({
                    "success": true,
                    "images": saved_images,
                    "prompt": prompt,
                    "source_image": image_filename,
                    "model": resp.model,
                    "provider": resp.provider,
                }))
            }
            Err(e) => ToolResult::err(format!("Image edit failed: {}", e)),
        }
    }
}

// ─── Generate Video ─────────────────────────────────────────────────────────

pub struct GenerateVideoTool;

#[async_trait]
impl NativeTool for GenerateVideoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_video".to_string(),
            display_name: "Generate Video".to_string(),
            description: "Generate a video from a text prompt. Video generation may take 30-120 seconds.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Detailed description of the video to generate"
                    },
                    "duration": {
                        "type": "integer",
                        "description": "Video duration in seconds (1-15, default: 5)",
                        "minimum": 1,
                        "maximum": 15
                    },
                    "aspect_ratio": {
                        "type": "string",
                        "enum": ["16:9", "9:16", "1:1"],
                        "description": "Video aspect ratio (default: 16:9)"
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["xai", "openai", "google", "huggingface"],
                        "description": "Provider to use (default: xai)"
                    }
                },
                "required": ["prompt"]
            }),
            instructions: Some(
                "Use generate_video when the user asks to create, generate, or make a video, \
                 clip, or animation. Warn the user that video generation takes 30-120 seconds. \
                 Keep prompts descriptive but concise."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let prompt = match args["prompt"].as_str() {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => return ToolResult::err("Missing required parameter: prompt"),
        };

        let (provider, api_key) = match get_provider_config(args, "video") {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let adaptor = match media::create_media_adaptor(&provider, &api_key, None) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("Failed to create media adaptor: {}", e)),
        };

        let req = media::VideoRequest {
            prompt: prompt.clone(),
            duration: args["duration"].as_u64().map(|d| d as u32),
            aspect_ratio: args["aspect_ratio"].as_str().map(|s| s.to_string()),
            image_url: args["image_url"].as_str().map(|s| s.to_string()),
        };

        match adaptor.generate_video(req).await {
            Ok(resp) => {
                let filename = unique_filename("vid_", &resp.format);
                let filepath = media_dir("videos").join(&filename);

                if let Err(e) = std::fs::write(&filepath, &resp.video_data) {
                    warn!("Failed to save video to disk: {}", e);
                }

                // Encode video as base64 for frontend playback
                let video_b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &resp.video_data,
                );

                info!("Generated video: {} ({:.1}s) via {}", filename, resp.duration, provider);

                ToolResult::ok(serde_json::json!({
                    "success": true,
                    "video": {
                        "filename": filename,
                        "path": filepath.to_string_lossy(),
                        "base64": video_b64,
                        "format": resp.format,
                        "duration": resp.duration,
                    },
                    "prompt": prompt,
                    "model": resp.model,
                    "provider": resp.provider,
                }))
            }
            Err(e) => ToolResult::err(format!("Video generation failed: {}", e)),
        }
    }
}

// ─── Text to Speech ─────────────────────────────────────────────────────────

pub struct TextToSpeechTool;

#[async_trait]
impl NativeTool for TextToSpeechTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "text_to_speech".to_string(),
            display_name: "Text to Speech".to_string(),
            description: "Convert text to spoken audio. Saves to the media folder and returns audio for playback.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to convert to speech"
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice ID — xAI: eve/ara/rex/sal/leo, OpenAI: alloy/nova/echo/onyx/shimmer/marin/cedar"
                    },
                    "speed": {
                        "type": "number",
                        "description": "Speed multiplier (0.5-2.0, default: 1.0)",
                        "minimum": 0.5,
                        "maximum": 2.0
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["xai", "openai", "google", "huggingface"],
                        "description": "Provider to use (default: xai)"
                    }
                },
                "required": ["text"]
            }),
            instructions: Some(
                "Use text_to_speech when the user asks to read text aloud, generate audio, \
                 create a voiceover, or convert text to speech. Choose an appropriate voice \
                 based on context."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let text = match args["text"].as_str() {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return ToolResult::err("Missing required parameter: text"),
        };

        let (provider, api_key) = match get_provider_config(args, "tts") {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let adaptor = match media::create_media_adaptor(&provider, &api_key, None) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("Failed to create media adaptor: {}", e)),
        };

        let req = media::TtsRequest {
            text: text.clone(),
            voice: args["voice"].as_str().map(|s| s.to_string()),
            speed: args["speed"].as_f64().map(|s| s as f32),
            format: Some("mp3".to_string()),
        };

        match adaptor.text_to_speech(req).await {
            Ok(resp) => {
                let filename = unique_filename("tts_", &resp.format);
                let filepath = media_dir("audio").join(&filename);

                if let Err(e) = std::fs::write(&filepath, &resp.audio_data) {
                    warn!("Failed to save audio to disk: {}", e);
                }

                // Encode audio as base64 for frontend playback
                let audio_b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &resp.audio_data,
                );

                info!("Generated TTS: {} ({:.1}s est.) via {}", filename,
                    resp.duration_estimate.unwrap_or(0.0), provider);

                ToolResult::ok(serde_json::json!({
                    "success": true,
                    "audio": {
                        "filename": filename,
                        "path": filepath.to_string_lossy(),
                        "base64": audio_b64,
                        "format": resp.format,
                        "duration_estimate": resp.duration_estimate,
                    },
                    "text_preview": if text.len() > 100 {
                        format!("{}...", &text[..100])
                    } else {
                        text.clone()
                    },
                    "provider": resp.provider,
                }))
            }
            Err(e) => ToolResult::err(format!("Text-to-speech failed: {}", e)),
        }
    }
}
