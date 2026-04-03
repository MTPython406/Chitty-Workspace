"""
Media Engine — local image, video, and TTS generation via diffusers/transformers.

Manages a single pipeline at a time (VRAM-constrained). Provides:
  - MediaModelRegistry: JSON-based registry for local model directories
  - MediaEngine: load/unload/generate for image, video, and TTS models

Used by inference_server.py alongside the existing InferenceEngine for GGUF text.
"""

import gc
import io
import json
import logging
import os
import struct
import time
import wave
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

logger = logging.getLogger("chitty-media")

# ---------------------------------------------------------------------------
# Aspect ratio → resolution mapping (standard diffusion resolutions)
# ---------------------------------------------------------------------------

ASPECT_RESOLUTIONS: Dict[str, Tuple[int, int]] = {
    "1:1":  (1024, 1024),
    "16:9": (1344, 768),
    "9:16": (768, 1344),
    "4:3":  (1152, 864),
    "3:4":  (864, 1152),
}

# Video resolutions (typically lower than image for memory)
VIDEO_ASPECT_RESOLUTIONS: Dict[str, Tuple[int, int]] = {
    "1:1":  (512, 512),
    "16:9": (720, 480),
    "9:16": (480, 720),
    "4:3":  (640, 480),
    "3:4":  (480, 640),
}

# ---------------------------------------------------------------------------
# Pipeline class detection heuristics
# ---------------------------------------------------------------------------

# Maps patterns found in model_index.json "_class_name" or directory names
# to the diffusers pipeline class to use.
PIPELINE_HEURISTICS = {
    # Image models
    "flux":                "FluxPipeline",
    "stable-diffusion-xl": "StableDiffusionXLPipeline",
    "sdxl":                "StableDiffusionXLPipeline",
    "stable-diffusion-3":  "StableDiffusion3Pipeline",
    "sd3":                 "StableDiffusion3Pipeline",
    "stable-diffusion":    "StableDiffusionPipeline",
    "kandinsky":           "KandinskyV22Pipeline",
    "pixart":              "PixArtAlphaPipeline",
    # Video models
    "cogvideox":           "CogVideoXPipeline",
    "cogvideo":            "CogVideoXPipeline",
    "wan":                 "WanPipeline",
    "ltx-video":           "LTXVideoPipeline",
    "ltx":                 "LTXVideoPipeline",
    "animatediff":         "AnimateDiffPipeline",
    "mochi":               "MochiPipeline",
    # TTS models
    "bark":                "BarkModel",
    "xtts":                "XTTS",
    "speecht5":            "SpeechT5ForTextToSpeech",
    "parler":              "ParlerTTSForConditionalGeneration",
}

# Which pipeline classes correspond to which model type
IMAGE_PIPELINES = {
    "FluxPipeline", "StableDiffusionXLPipeline", "StableDiffusion3Pipeline",
    "StableDiffusionPipeline", "KandinskyV22Pipeline", "PixArtAlphaPipeline",
}
VIDEO_PIPELINES = {
    "CogVideoXPipeline", "WanPipeline", "LTXVideoPipeline",
    "AnimateDiffPipeline", "MochiPipeline",
}
TTS_PIPELINES = {
    "BarkModel", "XTTS", "SpeechT5ForTextToSpeech",
    "ParlerTTSForConditionalGeneration",
}


def _pipeline_to_model_type(pipeline_class: str) -> str:
    """Infer model_type from pipeline class name."""
    if pipeline_class in IMAGE_PIPELINES:
        return "image"
    if pipeline_class in VIDEO_PIPELINES:
        return "video"
    if pipeline_class in TTS_PIPELINES:
        return "tts"
    return "image"  # default fallback


def _detect_pipeline_class(model_path: str) -> Optional[str]:
    """
    Auto-detect the diffusers pipeline class from a model directory.

    Checks model_index.json first (authoritative), then falls back to
    directory name heuristics.
    """
    model_dir = Path(model_path)

    # 1. Check model_index.json (diffusers standard)
    index_file = model_dir / "model_index.json"
    if index_file.exists():
        try:
            with open(index_file, "r") as f:
                index = json.load(f)
            class_name = index.get("_class_name")
            if class_name:
                return class_name
        except (json.JSONDecodeError, KeyError):
            pass

    # 2. Fallback: match directory name against heuristics
    dir_name = model_dir.name.lower()
    for pattern, pipeline in PIPELINE_HEURISTICS.items():
        if pattern in dir_name:
            return pipeline

    # 3. Check if it looks like a single-file model (safetensors/bin)
    safetensor_files = list(model_dir.glob("*.safetensors"))
    if safetensor_files and not index_file.exists():
        # Single checkpoint — likely needs a specific pipeline
        # Can't auto-detect; user should specify
        return None

    return None


def _get_dir_size(path: Path) -> int:
    """Get total size of a directory in bytes."""
    total = 0
    try:
        for entry in path.rglob("*"):
            if entry.is_file():
                total += entry.stat().st_size
    except OSError:
        pass
    return total


# ---------------------------------------------------------------------------
# MediaModelRegistry
# ---------------------------------------------------------------------------

class MediaModelRegistry:
    """
    Registry of local media model directories (image, video, TTS).

    Persisted to JSON. Models are registered by path and tagged with
    their type and pipeline class.
    """

    def __init__(self, registry_path: Path):
        self.registry_path = registry_path
        self.models: Dict[str, Dict[str, Any]] = {}
        self._load()

    def _load(self):
        if self.registry_path.exists():
            try:
                with open(self.registry_path, "r") as f:
                    data = json.load(f)
                self.models = data.get("models", {})
            except (json.JSONDecodeError, KeyError):
                self.models = {}
        else:
            self.models = {}

    def _save(self):
        self.registry_path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            "models": self.models,
            "updated_at": int(time.time()),
        }
        with open(self.registry_path, "w") as f:
            json.dump(data, f, indent=2)

    def register(
        self,
        path: str,
        name: Optional[str] = None,
        model_type: Optional[str] = None,
        pipeline_class: Optional[str] = None,
    ) -> Dict[str, Any]:
        """
        Register a local model directory.

        Args:
            path: Absolute path to the model directory
            name: Display name (defaults to directory name)
            model_type: "image", "video", or "tts" (auto-detected if omitted)
            pipeline_class: Diffusers pipeline class (auto-detected if omitted)
        """
        model_dir = Path(path)
        if not model_dir.exists():
            raise ValueError(f"Model path does not exist: {path}")
        if not model_dir.is_dir():
            raise ValueError(f"Model path is not a directory: {path}")

        # Auto-detect pipeline class
        if not pipeline_class:
            pipeline_class = _detect_pipeline_class(path)

        # Auto-detect model type from pipeline class
        if not model_type and pipeline_class:
            model_type = _pipeline_to_model_type(pipeline_class)
        elif not model_type:
            model_type = "image"  # default

        # Generate name from directory name if not provided
        if not name:
            name = model_dir.name

        size_bytes = _get_dir_size(model_dir)

        entry = {
            "path": str(model_dir.resolve()),
            "name": name,
            "model_type": model_type,
            "pipeline_class": pipeline_class,
            "size_bytes": size_bytes,
            "size_gb": round(size_bytes / (1024 ** 3), 2),
            "dtype": "fp16",  # default, updated on load
        }

        self.models[name] = entry
        self._save()
        logger.info(f"Registered media model: {name} ({model_type}, {pipeline_class})")
        return entry

    def unregister(self, name: str) -> bool:
        if name in self.models:
            del self.models[name]
            self._save()
            logger.info(f"Unregistered media model: {name}")
            return True
        return False

    def list_models(self, model_type: Optional[str] = None) -> List[Dict[str, Any]]:
        models = list(self.models.values())
        if model_type:
            models = [m for m in models if m.get("model_type") == model_type]
        return models

    def get(self, name: str) -> Optional[Dict[str, Any]]:
        return self.models.get(name)


# ---------------------------------------------------------------------------
# MediaEngine
# ---------------------------------------------------------------------------

class MediaEngine:
    """
    Manages a single media pipeline (image/video/TTS) on GPU.

    Only one model can be loaded at a time. Loading a new model
    automatically unloads the previous one.
    """

    def __init__(self):
        self.pipeline = None
        self.loaded_model: Optional[str] = None
        self.loaded_type: Optional[str] = None
        self.loaded_pipeline_class: Optional[str] = None
        self._torch = None
        self._diffusers = None

    def _ensure_torch(self):
        if self._torch is None:
            import torch
            self._torch = torch

    def _ensure_diffusers(self):
        if self._diffusers is None:
            import diffusers
            self._diffusers = diffusers

    def load(
        self,
        model_path: str,
        model_name: str,
        model_type: str,
        pipeline_class: Optional[str] = None,
        dtype: str = "fp16",
    ):
        """Load a media model into GPU memory."""
        if self.loaded_model == model_name:
            logger.info(f"Media model {model_name} already loaded")
            return

        # Unload current model if any
        if self.pipeline is not None:
            self.unload()

        self._ensure_torch()
        torch = self._torch

        torch_dtype = torch.float16
        if dtype == "bf16":
            torch_dtype = torch.bfloat16
        elif dtype == "fp32" or dtype == "float32":
            torch_dtype = torch.float32

        t0 = time.time()
        logger.info(f"Loading media model: {model_name} ({model_type}, {pipeline_class}, {dtype})")

        if model_type == "tts":
            self._load_tts(model_path, pipeline_class, torch_dtype)
        elif model_type == "video":
            self._load_video(model_path, pipeline_class, torch_dtype)
        else:
            # Default: image
            self._load_image(model_path, pipeline_class, torch_dtype)

        self.loaded_model = model_name
        self.loaded_type = model_type
        self.loaded_pipeline_class = pipeline_class

        elapsed = time.time() - t0
        logger.info(f"Media model {model_name} loaded in {elapsed:.1f}s")

    def _load_image(self, model_path: str, pipeline_class: Optional[str], torch_dtype):
        """Load an image generation pipeline."""
        self._ensure_diffusers()
        diffusers = self._diffusers
        torch = self._torch

        # Try to use the specific pipeline class if known
        if pipeline_class and hasattr(diffusers, pipeline_class):
            PipelineClass = getattr(diffusers, pipeline_class)
        else:
            PipelineClass = diffusers.DiffusionPipeline

        self.pipeline = PipelineClass.from_pretrained(
            model_path,
            torch_dtype=torch_dtype,
            local_files_only=True,
        )

        # Move to GPU with memory optimizations
        if hasattr(self.pipeline, 'enable_model_cpu_offload'):
            try:
                self.pipeline.enable_model_cpu_offload()
            except Exception:
                # Fall back to direct GPU placement
                self.pipeline = self.pipeline.to("cuda")
        else:
            self.pipeline = self.pipeline.to("cuda")

        # Enable VAE optimizations for large images
        if hasattr(self.pipeline, 'enable_vae_slicing'):
            self.pipeline.enable_vae_slicing()
        if hasattr(self.pipeline, 'enable_vae_tiling'):
            self.pipeline.enable_vae_tiling()

    def _load_video(self, model_path: str, pipeline_class: Optional[str], torch_dtype):
        """Load a video generation pipeline."""
        self._ensure_diffusers()
        diffusers = self._diffusers
        torch = self._torch

        if pipeline_class and hasattr(diffusers, pipeline_class):
            PipelineClass = getattr(diffusers, pipeline_class)
        else:
            PipelineClass = diffusers.DiffusionPipeline

        self.pipeline = PipelineClass.from_pretrained(
            model_path,
            torch_dtype=torch_dtype,
            local_files_only=True,
        )

        if hasattr(self.pipeline, 'enable_model_cpu_offload'):
            try:
                self.pipeline.enable_model_cpu_offload()
            except Exception:
                self.pipeline = self.pipeline.to("cuda")
        else:
            self.pipeline = self.pipeline.to("cuda")

        if hasattr(self.pipeline, 'enable_vae_slicing'):
            self.pipeline.enable_vae_slicing()
        if hasattr(self.pipeline, 'enable_vae_tiling'):
            self.pipeline.enable_vae_tiling()

    def _load_tts(self, model_path: str, pipeline_class: Optional[str], torch_dtype):
        """Load a TTS model."""
        torch = self._torch

        if pipeline_class == "BarkModel":
            from transformers import BarkModel, BarkProcessor
            self.pipeline = {
                "model": BarkModel.from_pretrained(model_path, torch_dtype=torch_dtype).to("cuda"),
                "processor": BarkProcessor.from_pretrained(model_path),
                "type": "bark",
            }
        elif pipeline_class == "SpeechT5ForTextToSpeech":
            from transformers import SpeechT5ForTextToSpeech, SpeechT5Processor, SpeechT5HifiGan
            processor = SpeechT5Processor.from_pretrained(model_path)
            model = SpeechT5ForTextToSpeech.from_pretrained(model_path, torch_dtype=torch_dtype).to("cuda")
            # Try to load vocoder from a subfolder or default
            vocoder_path = Path(model_path) / "vocoder"
            if vocoder_path.exists():
                vocoder = SpeechT5HifiGan.from_pretrained(str(vocoder_path)).to("cuda")
            else:
                vocoder = SpeechT5HifiGan.from_pretrained("microsoft/speecht5_hifigan").to("cuda")
            self.pipeline = {
                "model": model,
                "processor": processor,
                "vocoder": vocoder,
                "type": "speecht5",
            }
        elif pipeline_class == "ParlerTTSForConditionalGeneration":
            from parler_tts import ParlerTTSForConditionalGeneration
            from transformers import AutoTokenizer
            model = ParlerTTSForConditionalGeneration.from_pretrained(
                model_path, torch_dtype=torch_dtype
            ).to("cuda")
            tokenizer = AutoTokenizer.from_pretrained(model_path)
            self.pipeline = {
                "model": model,
                "tokenizer": tokenizer,
                "type": "parler",
            }
        else:
            # Generic: try transformers AutoModel
            from transformers import AutoModel, AutoProcessor
            model = AutoModel.from_pretrained(model_path, torch_dtype=torch_dtype).to("cuda")
            processor = AutoProcessor.from_pretrained(model_path)
            self.pipeline = {
                "model": model,
                "processor": processor,
                "type": "generic_tts",
            }

    def unload(self):
        """Unload the current media model and free VRAM."""
        if self.pipeline is None:
            return

        model_name = self.loaded_model
        logger.info(f"Unloading media model: {model_name}")

        # Handle dict-style pipelines (TTS)
        if isinstance(self.pipeline, dict):
            for key, val in self.pipeline.items():
                if hasattr(val, 'to'):
                    try:
                        val.to("cpu")
                    except Exception:
                        pass
            self.pipeline.clear()
        else:
            try:
                self.pipeline.to("cpu")
            except Exception:
                pass

        del self.pipeline
        self.pipeline = None
        self.loaded_model = None
        self.loaded_type = None
        self.loaded_pipeline_class = None

        gc.collect()
        try:
            self._ensure_torch()
            if self._torch.cuda.is_available():
                self._torch.cuda.empty_cache()
        except Exception:
            pass

        logger.info(f"Media model {model_name} unloaded")

    # -------------------------------------------------------------------
    # Image generation
    # -------------------------------------------------------------------

    def generate_image(
        self,
        prompt: str,
        width: int = 1024,
        height: int = 1024,
        num_images: int = 1,
        steps: int = 30,
        guidance_scale: float = 7.5,
        seed: Optional[int] = None,
    ) -> List[bytes]:
        """
        Generate image(s) and return as list of PNG bytes.
        """
        if self.pipeline is None or self.loaded_type != "image":
            raise RuntimeError("No image model loaded. Load an image model first.")

        self._ensure_torch()
        torch = self._torch

        generator = None
        if seed is not None:
            generator = torch.Generator(device="cuda").manual_seed(seed)

        # Build kwargs — different pipelines accept different params
        kwargs = {
            "prompt": prompt,
            "num_inference_steps": steps,
            "num_images_per_prompt": num_images,
        }

        # Not all pipelines support guidance_scale (e.g., Flux guidance distilled)
        if guidance_scale > 0:
            kwargs["guidance_scale"] = guidance_scale

        # Set dimensions if pipeline supports them
        if hasattr(self.pipeline, '__call__'):
            kwargs["width"] = width
            kwargs["height"] = height

        if generator is not None:
            kwargs["generator"] = generator

        t0 = time.time()
        logger.info(f"Generating {num_images} image(s): {width}x{height}, {steps} steps")

        try:
            result = self.pipeline(**kwargs)
        except TypeError as e:
            # Some pipelines don't accept certain kwargs — retry with minimal set
            logger.warning(f"Pipeline call failed with kwargs, retrying minimal: {e}")
            minimal_kwargs = {"prompt": prompt, "num_inference_steps": steps}
            if generator:
                minimal_kwargs["generator"] = generator
            result = self.pipeline(**minimal_kwargs)

        elapsed = time.time() - t0
        logger.info(f"Image generation completed in {elapsed:.1f}s")

        # Extract PIL images from result
        images = result.images if hasattr(result, 'images') else [result]

        # Convert PIL images to PNG bytes
        png_bytes_list = []
        for img in images:
            buf = io.BytesIO()
            img.save(buf, format="PNG")
            png_bytes_list.append(buf.getvalue())

        return png_bytes_list

    # -------------------------------------------------------------------
    # Video generation
    # -------------------------------------------------------------------

    def generate_video(
        self,
        prompt: str,
        width: int = 720,
        height: int = 480,
        num_frames: int = 49,
        steps: int = 50,
        guidance_scale: float = 6.0,
        seed: Optional[int] = None,
    ) -> bytes:
        """
        Generate a video and return as MP4 bytes.
        """
        if self.pipeline is None or self.loaded_type != "video":
            raise RuntimeError("No video model loaded. Load a video model first.")

        self._ensure_torch()
        torch = self._torch

        generator = None
        if seed is not None:
            generator = torch.Generator(device="cuda").manual_seed(seed)

        kwargs = {
            "prompt": prompt,
            "num_inference_steps": steps,
            "guidance_scale": guidance_scale,
            "num_frames": num_frames,
            "width": width,
            "height": height,
        }
        if generator is not None:
            kwargs["generator"] = generator

        t0 = time.time()
        logger.info(f"Generating video: {width}x{height}, {num_frames} frames, {steps} steps")

        try:
            result = self.pipeline(**kwargs)
        except TypeError as e:
            logger.warning(f"Video pipeline call failed, retrying minimal: {e}")
            minimal_kwargs = {
                "prompt": prompt,
                "num_inference_steps": steps,
                "num_frames": num_frames,
            }
            if generator:
                minimal_kwargs["generator"] = generator
            result = self.pipeline(**minimal_kwargs)

        elapsed = time.time() - t0
        logger.info(f"Video generation completed in {elapsed:.1f}s")

        # Extract frames and export to MP4
        frames = result.frames if hasattr(result, 'frames') else result

        # Use diffusers export utility
        try:
            from diffusers.utils import export_to_video
            import tempfile

            # If frames is a list of lists (batch), take first
            if isinstance(frames, (list, tuple)) and len(frames) > 0:
                if isinstance(frames[0], (list, tuple)):
                    frames = frames[0]

            with tempfile.NamedTemporaryFile(suffix=".mp4", delete=False) as tmp:
                tmp_path = tmp.name

            export_to_video(frames, tmp_path, fps=8)

            with open(tmp_path, "rb") as f:
                mp4_bytes = f.read()

            os.unlink(tmp_path)
            return mp4_bytes

        except ImportError:
            raise RuntimeError(
                "diffusers.utils.export_to_video not available. "
                "Update diffusers: pip install --upgrade diffusers"
            )

    # -------------------------------------------------------------------
    # Text-to-speech
    # -------------------------------------------------------------------

    def text_to_speech(
        self,
        text: str,
        voice: Optional[str] = None,
        speed: float = 1.0,
    ) -> Tuple[bytes, int]:
        """
        Generate speech audio. Returns (WAV bytes, sample_rate).
        """
        if self.pipeline is None or self.loaded_type != "tts":
            raise RuntimeError("No TTS model loaded. Load a TTS model first.")

        self._ensure_torch()
        torch = self._torch

        t0 = time.time()
        logger.info(f"Generating TTS: {len(text)} chars, voice={voice}")

        if isinstance(self.pipeline, dict):
            tts_type = self.pipeline.get("type", "generic_tts")

            if tts_type == "bark":
                audio_array, sample_rate = self._tts_bark(text, voice)
            elif tts_type == "speecht5":
                audio_array, sample_rate = self._tts_speecht5(text, voice)
            elif tts_type == "parler":
                audio_array, sample_rate = self._tts_parler(text, voice)
            else:
                raise RuntimeError(f"Unsupported TTS type: {tts_type}")
        else:
            raise RuntimeError("TTS pipeline is not properly loaded")

        elapsed = time.time() - t0
        logger.info(f"TTS completed in {elapsed:.1f}s, {len(audio_array)} samples @ {sample_rate}Hz")

        # Convert to WAV bytes
        wav_bytes = self._audio_to_wav(audio_array, sample_rate)
        return wav_bytes, sample_rate

    def _tts_bark(self, text: str, voice: Optional[str]):
        """Generate speech using Bark model."""
        import numpy as np

        model = self.pipeline["model"]
        processor = self.pipeline["processor"]

        voice_preset = voice or "v2/en_speaker_6"
        inputs = processor(text, voice_preset=voice_preset, return_tensors="pt").to("cuda")

        with self._torch.no_grad():
            output = model.generate(**inputs)

        audio = output.cpu().numpy().squeeze()
        sample_rate = model.generation_config.sample_rate
        return audio, sample_rate

    def _tts_speecht5(self, text: str, voice: Optional[str]):
        """Generate speech using SpeechT5."""
        import numpy as np

        model = self.pipeline["model"]
        processor = self.pipeline["processor"]
        vocoder = self.pipeline["vocoder"]

        inputs = processor(text=text, return_tensors="pt").to("cuda")

        # SpeechT5 needs speaker embeddings
        # Use a default embedding if no voice specified
        speaker_embeddings = self._torch.zeros(1, 512).to("cuda")

        with self._torch.no_grad():
            speech = model.generate_speech(
                inputs["input_ids"],
                speaker_embeddings,
                vocoder=vocoder,
            )

        audio = speech.cpu().numpy()
        return audio, 16000  # SpeechT5 default sample rate

    def _tts_parler(self, text: str, voice: Optional[str]):
        """Generate speech using Parler TTS."""
        import numpy as np

        model = self.pipeline["model"]
        tokenizer = self.pipeline["tokenizer"]

        description = voice or "A female speaker with a clear and natural voice."
        input_ids = tokenizer(description, return_tensors="pt").input_ids.to("cuda")
        prompt_input_ids = tokenizer(text, return_tensors="pt").input_ids.to("cuda")

        with self._torch.no_grad():
            generation = model.generate(input_ids=input_ids, prompt_input_ids=prompt_input_ids)

        audio = generation.cpu().numpy().squeeze()
        sample_rate = model.config.sampling_rate
        return audio, sample_rate

    def _audio_to_wav(self, audio_array, sample_rate: int) -> bytes:
        """Convert a numpy audio array to WAV bytes."""
        import numpy as np

        # Normalize to int16 range
        if audio_array.dtype == np.float32 or audio_array.dtype == np.float64:
            audio_array = np.clip(audio_array, -1.0, 1.0)
            audio_int16 = (audio_array * 32767).astype(np.int16)
        else:
            audio_int16 = audio_array.astype(np.int16)

        buf = io.BytesIO()
        with wave.open(buf, "wb") as wf:
            wf.setnchannels(1)  # mono
            wf.setsampwidth(2)  # 16-bit
            wf.setframerate(sample_rate)
            wf.writeframes(audio_int16.tobytes())

        return buf.getvalue()
