"""
Chitty Workspace Inference Server — Python sidecar for local model inference.

Managed by the Chitty Workspace Rust binary as a child process.
Provides a REST API for:
  - GGUF model loading and chat completions via llama-cpp-python
  - Image generation via diffusers (SDXL, Flux, Stable Diffusion, SD3)
  - Video generation via diffusers (CogVideoX, Wan, LTX-Video)
  - Text-to-speech via transformers (Bark, SpeechT5, Parler)

Usage:
    python inference_server.py --port 8766
    python inference_server.py --port 8766 --models-dir "C:/LLM Models"
"""

import argparse
import base64
import gc
import glob
import io
import json
import logging
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

# ── CUDA DLL setup (must happen before any llama_cpp import) ──────────
# The nvidia-cuda-runtime-cu12 and nvidia-cublas-cu12 pip packages install
# CUDA runtime DLLs under site-packages/nvidia/*/bin/. We need these on
# PATH so that llama_cpp's ggml-cuda.dll can find cublas64, cudart64, etc.
def _setup_cuda_dll_paths():
    """Add NVIDIA CUDA DLL directories to PATH and os.add_dll_directory."""
    site_pkgs = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'venv', 'Lib', 'site-packages')
    if not os.path.isdir(site_pkgs):
        # Fallback: use the running interpreter's site-packages
        import site
        for sp in site.getsitepackages():
            if os.path.isdir(os.path.join(sp, 'nvidia')):
                site_pkgs = sp
                break

    dirs_to_add = []

    # 1. System CUDA Toolkit (installed via CUDA Toolkit installer)
    cuda_path = os.environ.get('CUDA_PATH', '')
    if not cuda_path:
        # Auto-detect CUDA Toolkit installation
        for ver_dir in sorted(glob.glob('C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v*'), reverse=True):
            cuda_path = ver_dir
            break
    if cuda_path:
        cuda_bin_x64 = os.path.join(cuda_path, 'bin', 'x64')
        cuda_bin = os.path.join(cuda_path, 'bin')
        if os.path.isdir(cuda_bin_x64):
            dirs_to_add.append(cuda_bin_x64)
        if os.path.isdir(cuda_bin):
            dirs_to_add.append(cuda_bin)

    # 2. Pip-installed nvidia CUDA runtime packages (fallback)
    for d in glob.glob(os.path.join(site_pkgs, 'nvidia', '*', 'bin')):
        dirs_to_add.append(d)

    # 3. llama_cpp lib dir (contains ggml-cuda.dll, llama.dll, etc.)
    llama_lib = os.path.join(site_pkgs, 'llama_cpp', 'lib')
    if os.path.isdir(llama_lib):
        dirs_to_add.append(llama_lib)

    # Add all directories to PATH and os.add_dll_directory
    for d in dirs_to_add:
        os.environ['PATH'] = d + os.pathsep + os.environ.get('PATH', '')
        if hasattr(os, 'add_dll_directory'):
            try:
                os.add_dll_directory(d)
            except OSError:
                pass

_setup_cuda_dll_paths()

from fastapi import FastAPI, HTTPException
from fastapi.responses import JSONResponse, StreamingResponse
from pydantic import BaseModel, Field
import uvicorn

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_PORT = 8766
DATA_DIR = Path.home() / ".chitty-workspace"
MODELS_DIR = DATA_DIR / "models"
REGISTRY_FILE = DATA_DIR / "hf_models.json"
MEDIA_REGISTRY_FILE = DATA_DIR / "hf_media_models.json"

# Additional search directories for GGUF files (configurable via --models-dir)
EXTRA_MODEL_DIRS: List[Path] = []

logger = logging.getLogger("chitty-inference")
logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(name)s] %(levelname)s: %(message)s")

# ---------------------------------------------------------------------------
# Pydantic Schemas
# ---------------------------------------------------------------------------

class RegisterModelRequest(BaseModel):
    path: str = Field(..., description="Absolute path to the GGUF model file")
    name: Optional[str] = Field(None, description="Display name (defaults to filename stem)")

class UnregisterModelRequest(BaseModel):
    name: str = Field(..., description="Model name to remove from registry")

class LoadModelRequest(BaseModel):
    model: str = Field(..., description="Model name to load")
    gpu_layers: int = Field(-1, description="Number of GPU layers (-1 = all)")
    context_length: int = Field(32768, description="Context window size")

class ChatMessage(BaseModel):
    role: str
    content: Optional[str] = None
    tool_calls: Optional[List[Dict[str, Any]]] = None
    tool_call_id: Optional[str] = None
    name: Optional[str] = None

class ToolFunction(BaseModel):
    name: str
    description: Optional[str] = None
    parameters: Optional[Dict[str, Any]] = None

class ToolDefinition(BaseModel):
    type: str = "function"
    function: ToolFunction

class ChatCompletionRequest(BaseModel):
    model: str
    messages: List[ChatMessage]
    temperature: float = 0.7
    max_tokens: int = 65536
    top_p: float = 0.8
    top_k: int = 20
    repetition_penalty: float = 1.05
    stream: bool = False
    tools: Optional[List[ToolDefinition]] = None
    tool_choice: Optional[Any] = None

class ModelInfo(BaseModel):
    name: str
    path: str
    size_bytes: int
    size_gb: float
    quantization: Optional[str] = None
    loaded: bool = False


# ---------------------------------------------------------------------------
# Media model Pydantic schemas
# ---------------------------------------------------------------------------

class RegisterMediaModelRequest(BaseModel):
    path: str = Field(..., description="Absolute path to the model directory")
    name: Optional[str] = Field(None, description="Display name (defaults to directory name)")
    model_type: Optional[str] = Field(None, description="Model type: 'image', 'video', or 'tts' (auto-detected)")
    pipeline_class: Optional[str] = Field(None, description="Diffusers pipeline class (auto-detected)")

class UnregisterMediaModelRequest(BaseModel):
    name: str = Field(..., description="Media model name to remove from registry")

class LoadMediaModelRequest(BaseModel):
    model: str = Field(..., description="Media model name to load")
    dtype: str = Field("fp16", description="Data type: 'fp16', 'bf16', or 'fp32'")

class GenerateImageRequest(BaseModel):
    prompt: str = Field(..., description="Image generation prompt")
    n: int = Field(1, ge=1, le=4, description="Number of images (1-4)")
    aspect_ratio: str = Field("1:1", description="Aspect ratio: '1:1', '16:9', '9:16', '4:3', '3:4'")
    steps: int = Field(30, ge=1, le=150, description="Inference steps")
    guidance_scale: float = Field(7.5, ge=0.0, le=30.0, description="Guidance scale (0 = no guidance)")
    seed: Optional[int] = Field(None, description="Random seed for reproducibility")

class GenerateVideoRequest(BaseModel):
    prompt: str = Field(..., description="Video generation prompt")
    num_frames: int = Field(49, ge=8, le=256, description="Number of frames")
    aspect_ratio: str = Field("16:9", description="Aspect ratio: '16:9', '9:16', '1:1'")
    steps: int = Field(50, ge=1, le=150, description="Inference steps")
    guidance_scale: float = Field(6.0, ge=0.0, le=30.0, description="Guidance scale")
    seed: Optional[int] = Field(None, description="Random seed for reproducibility")

class MediaTTSRequest(BaseModel):
    text: str = Field(..., description="Text to synthesize")
    voice: Optional[str] = Field(None, description="Voice preset or speaker description")
    speed: float = Field(1.0, ge=0.5, le=2.0, description="Speed multiplier")
    format: str = Field("wav", description="Output format: 'wav'")

class MediaModelInfo(BaseModel):
    name: str
    path: str
    model_type: str
    pipeline_class: Optional[str] = None
    size_bytes: int
    size_gb: float
    dtype: Optional[str] = None
    loaded: bool = False


# ---------------------------------------------------------------------------
# Model Registry — persisted to ~/.chitty-workspace/hf_models.json
# ---------------------------------------------------------------------------

class ModelRegistry:
    """Tracks registered GGUF model files."""

    def __init__(self, registry_path: Path = REGISTRY_FILE, models_dirs: Optional[List[Path]] = None):
        self.registry_path = registry_path
        self.models_dirs = models_dirs or [MODELS_DIR]
        self.models: Dict[str, Dict[str, Any]] = {}
        self._load()

    def _load(self):
        """Load registry from disk."""
        if self.registry_path.exists():
            try:
                data = json.loads(self.registry_path.read_text(encoding="utf-8"))
                self.models = data.get("models", {})
                logger.info(f"Loaded {len(self.models)} models from registry")
            except Exception as e:
                logger.warning(f"Failed to load registry: {e}")
                self.models = {}
        # Auto-discover GGUF files in all model directories
        self._discover_models()

    def _discover_models(self):
        """Scan all model directories for GGUF files not yet registered."""
        discovered = 0
        for models_dir in self.models_dirs:
            if not models_dir.exists():
                models_dir.mkdir(parents=True, exist_ok=True)
                continue
            for gguf_file in models_dir.glob("*.gguf"):
                name = gguf_file.stem
                if name not in self.models:
                    self.models[name] = {
                        "path": str(gguf_file),
                        "name": name,
                        "size_bytes": gguf_file.stat().st_size,
                        "quantization": _detect_quantization(name),
                        "capabilities": _detect_capabilities(name),
                    }
                    discovered += 1
        if discovered:
            logger.info(f"Auto-discovered {discovered} GGUF files")
            self._save()

    def _save(self):
        """Persist registry to disk."""
        self.registry_path.parent.mkdir(parents=True, exist_ok=True)
        data = {"models": self.models, "updated_at": time.time()}
        self.registry_path.write_text(json.dumps(data, indent=2), encoding="utf-8")

    def register(self, path: str, name: Optional[str] = None) -> Dict[str, Any]:
        """Register a GGUF model file."""
        p = Path(path)
        if not p.exists():
            raise FileNotFoundError(f"Model file not found: {path}")
        if not p.suffix.lower() == ".gguf":
            raise ValueError(f"Only GGUF files are supported, got: {p.suffix}")

        model_name = name or p.stem
        self.models[model_name] = {
            "path": str(p.resolve()),
            "name": model_name,
            "size_bytes": p.stat().st_size,
            "quantization": _detect_quantization(p.stem),
            "capabilities": _detect_capabilities(p.stem),
        }
        self._save()
        logger.info(f"Registered model: {model_name} -> {path}")
        return self.models[model_name]

    def unregister(self, name: str) -> bool:
        """Remove a model from registry (does not delete the file)."""
        if name in self.models:
            del self.models[name]
            self._save()
            logger.info(f"Unregistered model: {name}")
            return True
        return False

    def rescan(self) -> int:
        """Force re-scan all directories. Returns count of newly discovered models."""
        before = len(self.models)
        self._discover_models()
        return len(self.models) - before

    def list_models(self) -> List[Dict[str, Any]]:
        """Return all registered models."""
        return list(self.models.values())

    def get(self, name: str) -> Optional[Dict[str, Any]]:
        """Get a model by name."""
        return self.models.get(name)


def _detect_quantization(filename: str) -> Optional[str]:
    """Detect quantization from GGUF filename conventions."""
    upper = filename.upper()
    quant_patterns = [
        "Q2_K", "Q3_K_S", "Q3_K_M", "Q3_K_L",
        "Q4_0", "Q4_1", "Q4_K_S", "Q4_K_M",
        "Q5_0", "Q5_1", "Q5_K_S", "Q5_K_M",
        "Q6_K", "Q8_0", "F16", "F32",
        "IQ1_S", "IQ1_M", "IQ2_XXS", "IQ2_XS", "IQ2_S", "IQ2_M",
        "IQ3_XXS", "IQ3_XS", "IQ3_S", "IQ4_XS", "IQ4_NL",
    ]
    for q in quant_patterns:
        if q in upper:
            return q
    return None


def _detect_capabilities(filename: str) -> List[str]:
    """Detect model capabilities from the GGUF filename."""
    lower = filename.lower()
    caps = ["text"]  # All models can do text

    # Tool/function calling capable models
    tool_models = [
        "qwen2.5", "qwen3", "llama-3.1", "llama-3.2", "llama-3.3",
        "mistral", "functionary", "hermes", "nexusraven",
        "gorilla", "gemma-2", "gemma-4", "phi-3", "phi-4", "command-r",
    ]
    if any(m in lower for m in tool_models):
        caps.append("tools")

    # Coding specialists
    code_models = ["coder", "codellama", "starcoder", "deepseek-coder", "qwen3-coder"]
    if any(m in lower for m in code_models):
        caps.append("code")

    # Vision models
    vision_models = ["-vl-", "-vl.", "llava", "vision", "minicpm-v", "moondream"]
    if any(m in lower for m in vision_models):
        caps.append("vision")

    # Reasoning/thinking models (not great at tools)
    reasoning_models = ["deepseek-r1", "qwq"]
    if any(m in lower for m in reasoning_models):
        caps.append("reasoning")
        if "tools" in caps:
            caps.remove("tools")  # R1 distills are bad at tool calling

    return caps


# ---------------------------------------------------------------------------
# Fallback Tool Call Parser — extracts tool calls from model text output
# ---------------------------------------------------------------------------

import re
import uuid

def _parse_tool_calls_from_text(content: str):
    """
    Parse tool calls that models output as text instead of structured tool_calls.
    Supports multiple formats:
      1. XML: <tool_call><function=name><parameter=key>value</parameter></function></tool_call>
      2. JSON in tags: <tool_call>{"name": "...", "arguments": {...}}</tool_call>
      3. Bare JSON: {"name": "...", "parameters": {...}}
      4. Qwen function_call: <tool_call>\n{"name": "...", "arguments": {...}}\n</tool_call>

    Returns: (tool_calls_list, cleaned_content) or (None, content) if no tool calls found.
    """
    tool_calls = []
    cleaned = content

    # Pattern 1: XML-style tool calls — multiple formats from GGUF models:
    #   a) <tool_call><function=name>...</function></tool_call>  (full wrapper)
    #   b) <function=name>...</function></tool_call>              (no opening tag)
    #   c) <function=name>...</function>                          (no wrapper at all)
    xml_pattern = r'(?:<tool_call>\s*)?<function=(\w+)>(.*?)</function>\s*(?:</tool_call>)?'
    xml_matches = re.findall(xml_pattern, content, re.DOTALL)
    if xml_matches:
        for func_name, params_block in xml_matches:
            args = {}
            param_pattern = r'<parameter=(\w+)>\s*(.*?)\s*</parameter>'
            for key, value in re.findall(param_pattern, params_block, re.DOTALL):
                # Try to parse value as JSON, otherwise use as string
                value = value.strip()
                try:
                    args[key] = json.loads(value)
                except (json.JSONDecodeError, ValueError):
                    args[key] = value
            tool_calls.append({
                "id": f"call_{uuid.uuid4().hex[:12]}",
                "type": "function",
                "function": {
                    "name": func_name,
                    "arguments": json.dumps(args),
                },
            })
        # Remove XML tool calls from content
        cleaned = re.sub(xml_pattern, '', cleaned, flags=re.DOTALL)

    # Pattern 2: JSON inside <tool_call> tags
    if not tool_calls:
        json_tag_pattern = r'<tool_call>\s*(\{.*?\})\s*</tool_call>'
        json_tag_matches = re.findall(json_tag_pattern, content, re.DOTALL)
        for json_str in json_tag_matches:
            try:
                # Handle doubled braces: {{"name": ...}} → {"name": ...}
                fixed = json_str.strip()
                if fixed.startswith('{{') and fixed.endswith('}}'):
                    fixed = fixed[1:-1]
                obj = json.loads(fixed)
                func_name = obj.get("name", "")
                args = obj.get("arguments", obj.get("parameters", {}))
                if func_name:
                    tool_calls.append({
                        "id": f"call_{uuid.uuid4().hex[:12]}",
                        "type": "function",
                        "function": {
                            "name": func_name,
                            "arguments": json.dumps(args) if isinstance(args, dict) else str(args),
                        },
                    })
            except (json.JSONDecodeError, ValueError):
                continue
        if tool_calls:
            cleaned = re.sub(json_tag_pattern, '', cleaned, flags=re.DOTALL)

    # Pattern 3: Bare JSON tool calls (no tags)
    # Match {"name": "tool_name", "parameters": {...}} or {"name": "tool_name", "arguments": {...}}
    if not tool_calls:
        bare_json_pattern = r'\{[\s]*"name"[\s]*:[\s]*"(\w+)"[\s]*,[\s]*"(?:parameters|arguments)"[\s]*:[\s]*(\{[^{}]*\})[\s]*\}'
        bare_matches = re.finditer(bare_json_pattern, content)
        for m in bare_matches:
            func_name = m.group(1)
            try:
                args = json.loads(m.group(2))
                tool_calls.append({
                    "id": f"call_{uuid.uuid4().hex[:12]}",
                    "type": "function",
                    "function": {
                        "name": func_name,
                        "arguments": json.dumps(args),
                    },
                })
            except (json.JSONDecodeError, ValueError):
                continue
        if tool_calls:
            # Remove the JSON from content
            cleaned = re.sub(bare_json_pattern, '', cleaned)

    # Pattern 4: Gemma 4 format — various pipe-delimited tool call formats:
    #   <|tool_call>call:terminal{command:<|"|>ls -F<|"|>}<tool_call|>
    #   <|tool_call|>call:name{key:<|"|>value<|"|>}<tool_call|>
    # The model also appends <|tool_response>...<channel>thought junk after.
    if not tool_calls:
        # Match: <|tool_call> or <|tool_call|> ... call:NAME{PARAMS} ... <tool_call|> or end
        gemma4_pattern = r'<\|?tool_call\|?>\s*call:(\w+)\{(.*?)\}\s*(?:<\|?/?tool_call\|?>)'
        gemma4_matches = re.findall(gemma4_pattern, content, re.DOTALL)
        for func_name, params_str in gemma4_matches:
            args = {}
            # Clean Gemma 4 quote delimiters: <|"|> → "
            params_clean = params_str.replace('<|"|>', '"').replace("<|'|>", "'")
            # Try JSON parse first: {command:"ls -F"} → {"command":"ls -F"}
            try:
                # Add quotes around unquoted keys for JSON parsing
                import re as _re
                json_attempt = _re.sub(r'(\w+)\s*:', r'"\1":', params_clean)
                args = json.loads('{' + json_attempt + '}')
            except (json.JSONDecodeError, ValueError):
                # Fall back to key:value parsing
                # Match key:<|"|>value<|"|> or key:"value" or key:value
                kv_pattern = r'(\w+)\s*:\s*(?:"([^"]*?)"|([^,}]+))'
                for match in re.finditer(kv_pattern, params_clean):
                    key = match.group(1)
                    value = match.group(2) if match.group(2) is not None else match.group(3)
                    args[key] = value.strip()
            if func_name:
                tool_calls.append({
                    "id": f"call_{uuid.uuid4().hex[:12]}",
                    "type": "function",
                    "function": {
                        "name": func_name,
                        "arguments": json.dumps(args),
                    },
                })
        if tool_calls:
            # Clean everything from the tool call onwards (model adds fake response text)
            cleaned = re.sub(r'<\|?tool_call\|?>.*', '', cleaned, flags=re.DOTALL).strip()

    if tool_calls:
        return tool_calls, cleaned
    return None, content


# ---------------------------------------------------------------------------
# Inference Engine — wraps llama-cpp-python
# ---------------------------------------------------------------------------

class InferenceEngine:
    """Manages a single loaded GGUF model via llama-cpp-python."""

    def __init__(self):
        self.llm = None
        self.loaded_model: Optional[str] = None
        self.loaded_path: Optional[str] = None
        self._llama_cpp = None

    def _ensure_llama_cpp(self):
        """Lazy import llama-cpp-python."""
        if self._llama_cpp is None:
            try:
                import llama_cpp
                self._llama_cpp = llama_cpp
            except ImportError:
                raise RuntimeError(
                    "llama-cpp-python is not installed. "
                    "Install with: pip install llama-cpp-python"
                )

    def load(self, model_path: str, model_name: str,
             gpu_layers: int = -1, context_length: int = 32768):
        """Load a GGUF model into memory."""
        self._ensure_llama_cpp()

        # Unload current model if different
        if self.loaded_model and self.loaded_model != model_name:
            self.unload()

        if self.loaded_model == model_name:
            logger.info(f"Model {model_name} already loaded")
            return

        logger.info(f"Loading model: {model_name} from {model_path} "
                     f"(gpu_layers={gpu_layers}, ctx={context_length})")
        start = time.time()

        # Use requested GPU layers — let llama.cpp handle GPU detection internally.
        # Note: llama_supports_gpu_offload() only reports compile-time CUDA support
        # of the Python bindings, but GGUF loading may still use GPU via other backends.
        actual_gpu_layers = gpu_layers
        logger.info(f"Using n_gpu_layers={actual_gpu_layers}")

        try:
            self.llm = self._llama_cpp.Llama(
                model_path=model_path,
                n_ctx=context_length,
                n_gpu_layers=actual_gpu_layers,
                verbose=False,
            )
        except Exception as load_err:
            logger.error(f"Llama load failed: {type(load_err).__name__}: {load_err}")
            raise ValueError(f"Failed to load model from file: {model_path}") from load_err

        elapsed = time.time() - start
        self.loaded_model = model_name
        self.loaded_path = model_path
        logger.info(f"Model {model_name} loaded in {elapsed:.1f}s")

    def unload(self):
        """Unload the current model and free memory."""
        if self.llm is not None:
            model_name = self.loaded_model
            del self.llm
            self.llm = None
            self.loaded_model = None
            self.loaded_path = None
            gc.collect()
            try:
                import torch
                if torch.cuda.is_available():
                    torch.cuda.empty_cache()
            except Exception:
                pass
            logger.info(f"Unloaded model: {model_name}")

    def chat(self, messages: List[Dict[str, Any]],
             temperature: float = 0.7, max_tokens: int = 65536,
             top_p: float = 0.8, top_k: int = 20,
             repeat_penalty: float = 1.05,
             tools: Optional[List[Dict[str, Any]]] = None,
             tool_choice: Optional[Any] = None) -> Dict[str, Any]:
        """Run chat completion on the loaded model, with optional tool calling."""
        if self.llm is None:
            raise RuntimeError("No model loaded. Call /models/load first.")

        start = time.time()

        # Build kwargs — only pass tools if provided
        kwargs = dict(
            messages=messages,
            temperature=temperature,
            max_tokens=max_tokens,
            top_p=top_p,
            top_k=top_k,
            repeat_penalty=repeat_penalty,
        )
        if tools:
            kwargs["tools"] = tools
            if tool_choice is not None:
                kwargs["tool_choice"] = tool_choice

        response = self.llm.create_chat_completion(**kwargs)

        elapsed = time.time() - start

        # Extract content and tool_calls from response
        choice = response.get("choices", [{}])[0]
        message = choice.get("message", {})
        content = message.get("content", "")
        tool_calls = message.get("tool_calls")
        finish_reason = choice.get("finish_reason", "stop")

        # ---- Fallback: parse tool calls from text output ----
        # Many GGUF models output tool calls as text (XML or JSON) instead of
        # structured tool_calls. Detect and convert them.
        if not tool_calls and content and tools:
            parsed, cleaned = _parse_tool_calls_from_text(content)
            if parsed:
                tool_calls = parsed
                content = cleaned.strip()
                finish_reason = "tool_calls"
                logger.info(f"Fallback parsed {len(parsed)} tool call(s) from text")

        # Usage stats
        usage = response.get("usage", {})

        result = {
            "content": content,
            "model": self.loaded_model,
            "finish_reason": finish_reason,
            "usage": {
                "prompt_tokens": usage.get("prompt_tokens", 0),
                "completion_tokens": usage.get("completion_tokens", 0),
                "total_tokens": usage.get("total_tokens", 0),
            },
            "elapsed_seconds": round(elapsed, 2),
        }
        if tool_calls:
            result["tool_calls"] = tool_calls
        return result


# ---------------------------------------------------------------------------
# GPU Stats helper
# ---------------------------------------------------------------------------

def get_gpu_free_vram_mb() -> Optional[int]:
    """Query free VRAM via nvidia-smi. Returns None if unavailable."""
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=memory.free", "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        if result.returncode == 0:
            return int(result.stdout.strip().split("\n")[0])
    except Exception:
        pass
    return None


# ---------------------------------------------------------------------------
# FastAPI Application
# ---------------------------------------------------------------------------

app = FastAPI(title="Chitty Workspace Inference Server", version="0.1.0")
registry: Optional[ModelRegistry] = None
engine = InferenceEngine()

# Media engine (image, video, TTS via diffusers/transformers)
from media_engine import MediaModelRegistry, MediaEngine, ASPECT_RESOLUTIONS, VIDEO_ASPECT_RESOLUTIONS
media_registry: Optional[MediaModelRegistry] = None
media_engine = MediaEngine()

# Training engine (LoRA/QLoRA fine-tuning)
from training_engine import TrainingEngine, DatasetManager
training_engine = TrainingEngine()
dataset_manager = DatasetManager()


@app.get("/health")
async def health():
    """Health check — reports sidecar status."""
    return {
        "status": "ok",
        "loaded_model": engine.loaded_model,
        "models_registered": len(registry.models) if registry else 0,
        "loaded_media_model": media_engine.loaded_model,
        "loaded_media_type": media_engine.loaded_type,
        "media_models_registered": len(media_registry.models) if media_registry else 0,
        "vram_free_mb": get_gpu_free_vram_mb(),
        "training_active": training_engine.is_running(),
    }


@app.get("/models")
async def list_models():
    """List all registered model files."""
    models = []
    for m in registry.list_models():
        size_bytes = m.get("size_bytes", 0)
        models.append(ModelInfo(
            name=m["name"],
            path=m["path"],
            size_bytes=size_bytes,
            size_gb=round(size_bytes / (1024 ** 3), 2),
            quantization=m.get("quantization"),
            loaded=(m["name"] == engine.loaded_model),
        ))
    return {"models": [m.model_dump() for m in models]}


@app.post("/models/scan")
async def scan_models():
    """Force re-scan all model directories for new GGUF files."""
    new_count = registry.rescan()
    return {
        "success": True,
        "new_models": new_count,
        "total_models": len(registry.models),
        "models": registry.list_models(),
    }


@app.post("/models/register")
async def register_model(request: RegisterModelRequest):
    """Register a GGUF model file path."""
    try:
        model = registry.register(request.path, request.name)
        return {"success": True, "model": model}
    except (FileNotFoundError, ValueError) as e:
        raise HTTPException(status_code=400, detail=str(e))


@app.post("/models/unregister")
async def unregister_model(request: UnregisterModelRequest):
    """Remove a model from the registry."""
    if engine.loaded_model == request.name:
        engine.unload()
    removed = registry.unregister(request.name)
    if not removed:
        raise HTTPException(status_code=404, detail=f"Model '{request.name}' not found")
    return {"success": True}


@app.post("/models/load")
async def load_model(request: LoadModelRequest):
    """Load a registered model into GPU memory."""
    model_info = registry.get(request.model)
    if not model_info:
        raise HTTPException(status_code=404, detail=f"Model '{request.model}' not registered")

    # VRAM coordination: unload media model first if loaded
    if media_engine.loaded_model:
        logger.info(f"Unloading media model '{media_engine.loaded_model}' to free VRAM for text model")
        media_engine.unload()

    try:
        engine.load(
            model_path=model_info["path"],
            model_name=request.model,
            gpu_layers=request.gpu_layers,
            context_length=request.context_length,
        )
        return {
            "success": True,
            "model": request.model,
            "vram_free_mb": get_gpu_free_vram_mb(),
        }
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/models/unload")
async def unload_model():
    """Unload the current model from memory."""
    if engine.loaded_model is None:
        return {"success": True, "message": "No model loaded"}
    model_name = engine.loaded_model
    engine.unload()
    return {
        "success": True,
        "unloaded": model_name,
        "vram_free_mb": get_gpu_free_vram_mb(),
    }


# ---------------------------------------------------------------------------
# Media Model Management Endpoints
# ---------------------------------------------------------------------------

@app.get("/media/models")
async def list_media_models():
    """List all registered media models."""
    if not media_registry:
        return {"models": []}
    models = []
    for m in media_registry.list_models():
        models.append(MediaModelInfo(
            name=m["name"],
            path=m["path"],
            model_type=m.get("model_type", "image"),
            pipeline_class=m.get("pipeline_class"),
            size_bytes=m.get("size_bytes", 0),
            size_gb=m.get("size_gb", 0),
            dtype=m.get("dtype"),
            loaded=(m["name"] == media_engine.loaded_model),
        ))
    return {"models": [m.model_dump() for m in models]}


@app.post("/media/models/register")
async def register_media_model(request: RegisterMediaModelRequest):
    """Register a media model directory (image, video, or TTS)."""
    if not media_registry:
        raise HTTPException(status_code=500, detail="Media registry not initialized")
    try:
        model = media_registry.register(
            path=request.path,
            name=request.name,
            model_type=request.model_type,
            pipeline_class=request.pipeline_class,
        )
        return {"success": True, "model": model}
    except (ValueError, FileNotFoundError) as e:
        raise HTTPException(status_code=400, detail=str(e))


@app.post("/media/models/unregister")
async def unregister_media_model(request: UnregisterMediaModelRequest):
    """Remove a media model from the registry."""
    if not media_registry:
        raise HTTPException(status_code=500, detail="Media registry not initialized")
    if media_engine.loaded_model == request.name:
        media_engine.unload()
    removed = media_registry.unregister(request.name)
    if not removed:
        raise HTTPException(status_code=404, detail=f"Media model '{request.name}' not found")
    return {"success": True}


@app.post("/media/models/load")
async def load_media_model(request: LoadMediaModelRequest):
    """Load a media model into GPU memory. Auto-unloads text engine if loaded."""
    if not media_registry:
        raise HTTPException(status_code=500, detail="Media registry not initialized")

    model_info = media_registry.get(request.model)
    if not model_info:
        raise HTTPException(status_code=404, detail=f"Media model '{request.model}' not registered")

    # VRAM coordination: unload text model first if loaded
    if engine.loaded_model:
        logger.info(f"Unloading text model '{engine.loaded_model}' to free VRAM for media model")
        engine.unload()

    try:
        media_engine.load(
            model_path=model_info["path"],
            model_name=request.model,
            model_type=model_info.get("model_type", "image"),
            pipeline_class=model_info.get("pipeline_class"),
            dtype=request.dtype,
        )
        return {
            "success": True,
            "model": request.model,
            "model_type": model_info.get("model_type"),
            "vram_free_mb": get_gpu_free_vram_mb(),
        }
    except Exception as e:
        logger.error(f"Failed to load media model: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/media/models/unload")
async def unload_media_model():
    """Unload the current media model from memory."""
    if media_engine.loaded_model is None:
        return {"success": True, "message": "No media model loaded"}
    model_name = media_engine.loaded_model
    media_engine.unload()
    return {
        "success": True,
        "unloaded": model_name,
        "vram_free_mb": get_gpu_free_vram_mb(),
    }


# ---------------------------------------------------------------------------
# Media Generation Endpoints
# ---------------------------------------------------------------------------

@app.post("/media/generate/image")
async def generate_image(request: GenerateImageRequest):
    """Generate image(s) from a text prompt using the loaded image model."""
    if media_engine.loaded_model is None or media_engine.loaded_type != "image":
        raise HTTPException(
            status_code=400,
            detail="No image model loaded. Load an image model first via /media/models/load"
        )

    # Resolve aspect ratio to pixel dimensions
    width, height = ASPECT_RESOLUTIONS.get(request.aspect_ratio, (1024, 1024))

    try:
        png_bytes_list = media_engine.generate_image(
            prompt=request.prompt,
            width=width,
            height=height,
            num_images=request.n,
            steps=request.steps,
            guidance_scale=request.guidance_scale,
            seed=request.seed,
        )

        # Convert PNG bytes to base64
        images = []
        for png_bytes in png_bytes_list:
            images.append({
                "base64": base64.b64encode(png_bytes).decode("utf-8"),
                "format": "png",
            })

        return {
            "images": images,
            "model": media_engine.loaded_model,
            "provider": "huggingface",
        }
    except torch_oom_error():
        media_engine.unload()
        raise HTTPException(
            status_code=507,
            detail="GPU out of memory. Model unloaded. Try fewer steps, smaller resolution, or a smaller model."
        )
    except Exception as e:
        logger.error(f"Image generation failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/media/generate/video")
async def generate_video(request: GenerateVideoRequest):
    """Generate a video from a text prompt using the loaded video model."""
    if media_engine.loaded_model is None or media_engine.loaded_type != "video":
        raise HTTPException(
            status_code=400,
            detail="No video model loaded. Load a video model first via /media/models/load"
        )

    # Resolve aspect ratio to pixel dimensions
    width, height = VIDEO_ASPECT_RESOLUTIONS.get(request.aspect_ratio, (720, 480))

    try:
        mp4_bytes = media_engine.generate_video(
            prompt=request.prompt,
            width=width,
            height=height,
            num_frames=request.num_frames,
            steps=request.steps,
            guidance_scale=request.guidance_scale,
            seed=request.seed,
        )

        # Calculate approximate duration (default 8 fps for most video models)
        fps = 8
        duration = request.num_frames / fps

        return {
            "video_base64": base64.b64encode(mp4_bytes).decode("utf-8"),
            "format": "mp4",
            "duration": round(duration, 1),
            "model": media_engine.loaded_model,
            "provider": "huggingface",
        }
    except torch_oom_error():
        media_engine.unload()
        raise HTTPException(
            status_code=507,
            detail="GPU out of memory. Model unloaded. Try fewer frames, fewer steps, or a smaller model."
        )
    except Exception as e:
        logger.error(f"Video generation failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/media/generate/tts")
async def generate_tts(request: MediaTTSRequest):
    """Generate speech audio from text using the loaded TTS model."""
    if media_engine.loaded_model is None or media_engine.loaded_type != "tts":
        raise HTTPException(
            status_code=400,
            detail="No TTS model loaded. Load a TTS model first via /media/models/load"
        )

    try:
        wav_bytes, sample_rate = media_engine.text_to_speech(
            text=request.text,
            voice=request.voice,
            speed=request.speed,
        )

        # Estimate duration from WAV size (16-bit mono)
        duration_estimate = len(wav_bytes) / (sample_rate * 2)  # 2 bytes per sample

        return {
            "audio_base64": base64.b64encode(wav_bytes).decode("utf-8"),
            "format": "wav",
            "duration_estimate": round(duration_estimate, 1),
            "model": media_engine.loaded_model,
            "provider": "huggingface",
        }
    except Exception as e:
        logger.error(f"TTS generation failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


# ---------------------------------------------------------------------------
# Speech-to-Text (Whisper)
# ---------------------------------------------------------------------------

# Module-level cache for the Whisper pipeline
_whisper_pipeline = None
_whisper_model_id = None


@app.post("/media/generate/stt")
async def speech_to_text(request: SpeechToTextRequest):
    """Transcribe audio to text using Whisper."""
    global _whisper_pipeline, _whisper_model_id

    try:
        audio_bytes = base64.b64decode(request.audio_base64)
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid base64 audio data")

    try:
        import torch
        from transformers import pipeline

        model_id = request.model or "openai/whisper-large-v3-turbo"

        # Load or reuse the pipeline
        if _whisper_pipeline is None or _whisper_model_id != model_id:
            # Unload text/media models to free VRAM
            if engine.loaded_model:
                logger.info(f"Unloading text model '{engine.loaded_model}' for STT")
                engine.unload()
            if media_engine.loaded_model:
                logger.info(f"Unloading media model '{media_engine.loaded_model}' for STT")
                media_engine.unload()

            logger.info(f"Loading Whisper model: {model_id}")
            _whisper_pipeline = pipeline(
                "automatic-speech-recognition",
                model=model_id,
                torch_dtype=torch.float16,
                device="cuda" if torch.cuda.is_available() else "cpu",
            )
            _whisper_model_id = model_id
            logger.info(f"Whisper model loaded: {model_id}")

        # Write audio to temp file (pipeline expects file path or array)
        import tempfile
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as f:
            f.write(audio_bytes)
            temp_path = f.name

        try:
            generate_kwargs = {}
            if request.language:
                generate_kwargs["language"] = request.language
            if request.task == "translate":
                generate_kwargs["task"] = "translate"

            result = _whisper_pipeline(
                temp_path,
                return_timestamps=True,
                generate_kwargs=generate_kwargs if generate_kwargs else None,
            )
        finally:
            os.unlink(temp_path)

        text = result.get("text", "")
        chunks = result.get("chunks", [])

        return {
            "text": text.strip(),
            "chunks": chunks,
            "model": model_id,
            "provider": "local",
        }
    except Exception as e:
        logger.error(f"STT failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@app.post("/media/stt/unload")
async def unload_stt():
    """Unload the Whisper model to free VRAM."""
    global _whisper_pipeline, _whisper_model_id
    if _whisper_pipeline is not None:
        del _whisper_pipeline
        _whisper_pipeline = None
        _whisper_model_id = None
        gc.collect()
        try:
            import torch
            torch.cuda.empty_cache()
        except Exception:
            pass
        return {"success": True, "message": "Whisper model unloaded"}
    return {"success": True, "message": "No Whisper model was loaded"}


# ---------------------------------------------------------------------------
# Training endpoints (LoRA/QLoRA fine-tuning)
# ---------------------------------------------------------------------------

class SpeechToTextRequest(BaseModel):
    audio_base64: str = Field(..., description="Base64-encoded audio data (WAV, MP3, FLAC, etc.)")
    model: str = Field("openai/whisper-large-v3-turbo", description="Whisper model ID")
    language: Optional[str] = Field(None, description="Language code (e.g. 'en', 'es'). Auto-detected if omitted.")
    task: str = Field("transcribe", description="'transcribe' or 'translate' (translate to English)")


class StartTrainingRequest(BaseModel):
    base_model: str = Field(..., description="HuggingFace model ID or local path")
    dataset: str = Field(..., description="Dataset filename from /training/datasets")
    lora_r: int = Field(16, ge=1, le=256)
    lora_alpha: int = Field(32, ge=1, le=512)
    lora_dropout: float = Field(0.05, ge=0.0, le=0.5)
    target_modules: Optional[List[str]] = None
    learning_rate: float = Field(2e-4, gt=0, le=1.0)
    num_epochs: int = Field(3, ge=1, le=100)
    batch_size: int = Field(4, ge=1, le=64)
    gradient_accumulation_steps: int = Field(4, ge=1, le=128)
    max_seq_length: int = Field(512, ge=32, le=8192)
    warmup_ratio: float = Field(0.03, ge=0.0, le=0.5)
    quantization: str = Field("4bit", description="4bit, 8bit, or none")
    output_name: Optional[str] = None

class MergeAdapterRequest(BaseModel):
    job_id: str
    output_name: Optional[str] = None

class DatasetUploadRequest(BaseModel):
    filename: str
    data_base64: str


@app.post("/training/start")
async def start_training(request: StartTrainingRequest):
    """Start a LoRA/QLoRA training job in the background."""
    if training_engine.is_running():
        raise HTTPException(status_code=409, detail="A training job is already running")

    # Validate dataset exists
    datasets = dataset_manager.list_datasets()
    ds_match = next((d for d in datasets if d["name"] == request.dataset), None)
    if not ds_match:
        raise HTTPException(status_code=404, detail=f"Dataset '{request.dataset}' not found")

    # Unload inference and media models to free VRAM
    if engine.loaded_model:
        logger.info(f"Unloading text model '{engine.loaded_model}' for training")
        engine.unload()
    if media_engine.loaded_model:
        logger.info(f"Unloading media model '{media_engine.loaded_model}' for training")
        media_engine.unload()

    config = request.model_dump()
    config["dataset_path"] = ds_match["path"]

    result = training_engine.start_training(config)
    if not result["success"]:
        raise HTTPException(status_code=500, detail=result.get("error", "Failed to start training"))
    return result


@app.get("/training/status")
async def training_status():
    """Get current training progress."""
    return training_engine.get_progress()


@app.post("/training/stop")
async def stop_training():
    """Cancel the running training job."""
    return training_engine.stop_training()


@app.get("/training/jobs")
async def list_training_jobs():
    """List all training jobs (current + completed)."""
    return {"jobs": training_engine.list_jobs()}


@app.post("/training/datasets/upload")
async def upload_dataset(request: DatasetUploadRequest):
    """Upload a dataset file (base64-encoded)."""
    try:
        content = base64.b64decode(request.data_base64)
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid base64 data")
    return dataset_manager.upload(request.filename, content)


@app.get("/training/datasets")
async def list_datasets():
    """List available training datasets."""
    return {"datasets": dataset_manager.list_datasets()}


@app.delete("/training/datasets/{name}")
async def delete_dataset(name: str):
    """Delete a training dataset."""
    if dataset_manager.delete(name):
        return {"success": True}
    raise HTTPException(status_code=404, detail="Dataset not found")


@app.post("/training/merge")
async def merge_adapter(request: MergeAdapterRequest):
    """Merge a LoRA adapter into the base model."""
    if training_engine.is_running():
        raise HTTPException(status_code=409, detail="Cannot merge while training is running")

    # Unload models to free VRAM for merge
    if engine.loaded_model:
        engine.unload()
    if media_engine.loaded_model:
        media_engine.unload()

    result = training_engine.merge_adapter(request.job_id, request.output_name)
    if not result["success"]:
        raise HTTPException(status_code=500, detail=result.get("error", "Merge failed"))
    return result


@app.get("/training/adapters")
async def list_adapters():
    """List saved LoRA adapters."""
    return {"adapters": training_engine.list_adapters()}


@app.delete("/training/adapters/{job_id}")
async def delete_adapter(job_id: str):
    """Delete a saved adapter."""
    result = training_engine.delete_adapter(job_id)
    if not result["success"]:
        raise HTTPException(status_code=404, detail=result.get("error", "Not found"))
    return result


# ---------------------------------------------------------------------------


def torch_oom_error():
    """Return the torch OOM exception class, or a dummy if torch not available."""
    try:
        import torch
        return torch.cuda.OutOfMemoryError
    except (ImportError, AttributeError):
        # Return a class that will never match
        return type('_NeverMatch', (Exception,), {})


@app.post("/chat/completions")
async def chat_completions(request: ChatCompletionRequest):
    """OpenAI-compatible chat completion endpoint with streaming support."""
    if training_engine.is_running():
        raise HTTPException(status_code=503, detail="Inference unavailable during training. Stop training first.")
    # Auto-load if model specified and not loaded
    if engine.loaded_model != request.model:
        model_info = registry.get(request.model)
        if not model_info:
            raise HTTPException(
                status_code=404,
                detail=f"Model '{request.model}' not registered. Register it first via /models/register"
            )
        try:
            engine.load(model_info["path"], request.model)
        except Exception as e:
            raise HTTPException(status_code=500, detail=f"Failed to load model: {e}")

    if engine.llm is None:
        raise HTTPException(status_code=500, detail="No model loaded")

    # Build messages — preserve tool_calls and tool role for multi-turn
    messages = []
    for m in request.messages:
        msg = {"role": m.role}
        if m.content is not None:
            msg["content"] = m.content
        if m.tool_calls:
            msg["tool_calls"] = m.tool_calls
        if m.tool_call_id:
            msg["tool_call_id"] = m.tool_call_id
        if m.name:
            msg["name"] = m.name
        # Ensure content key exists for basic messages
        if "content" not in msg:
            msg["content"] = ""
        messages.append(msg)

    # Build tools list for llama-cpp-python
    tools_list = None
    if request.tools:
        tools_list = [t.model_dump() for t in request.tools]

    if request.stream:
        # Streaming response — Server-Sent Events format
        def generate_stream():
            import uuid as _uuid
            chat_id = f"chatcmpl-{_uuid.uuid4().hex[:12]}"
            try:
                kwargs = dict(
                    messages=messages,
                    temperature=request.temperature,
                    max_tokens=request.max_tokens,
                    top_p=request.top_p,
                    top_k=request.top_k,
                    repeat_penalty=request.repetition_penalty,
                    stream=True,
                )
                if tools_list:
                    kwargs["tools"] = tools_list
                    if request.tool_choice is not None:
                        kwargs["tool_choice"] = request.tool_choice
                response = engine.llm.create_chat_completion(**kwargs)

                # Accumulate text so we can fallback-parse tool calls if needed
                accumulated_text = []
                had_structured_tool_calls = False
                last_finish_reason = None

                for chunk in response:
                    delta = chunk.get("choices", [{}])[0].get("delta", {})
                    finish_reason = chunk.get("choices", [{}])[0].get("finish_reason")
                    if finish_reason:
                        last_finish_reason = finish_reason

                    # Track whether the model is emitting structured tool_calls
                    if delta.get("tool_calls"):
                        had_structured_tool_calls = True

                    # Accumulate text content for fallback parsing
                    if delta.get("content"):
                        accumulated_text.append(delta["content"])

                    sse_chunk = {
                        "id": chat_id,
                        "object": "chat.completion.chunk",
                        "model": engine.loaded_model or request.model,
                        "choices": [{
                            "index": 0,
                            "delta": delta,
                            "finish_reason": finish_reason,
                        }],
                    }
                    yield f"data: {json.dumps(sse_chunk)}\n\n"

                # ---- Fallback: if model emitted tool calls as text, parse and re-emit ----
                # Many GGUF models output <tool_call>...</tool_call> as text instead of
                # structured tool_calls deltas. Detect and emit proper tool call chunks.
                if tools_list and not had_structured_tool_calls and accumulated_text:
                    full_text = "".join(accumulated_text)
                    parsed, cleaned = _parse_tool_calls_from_text(full_text)
                    if parsed:
                        logger.info(f"Stream fallback: parsed {len(parsed)} tool call(s) from text")
                        # Emit tool call chunks so the client sees structured tool calls
                        for tc in parsed:
                            fn = tc.get("function", {})
                            # ToolCallStart equivalent
                            start_delta = {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": tc["id"],
                                    "type": "function",
                                    "function": {"name": fn.get("name", ""), "arguments": ""},
                                }]
                            }
                            yield f"data: {json.dumps({'id': chat_id, 'object': 'chat.completion.chunk', 'model': engine.loaded_model or request.model, 'choices': [{'index': 0, 'delta': start_delta, 'finish_reason': None}]})}\n\n"
                            # ToolCallDelta with arguments
                            args_delta = {
                                "tool_calls": [{
                                    "index": 0,
                                    "function": {"arguments": fn.get("arguments", "{}")},
                                }]
                            }
                            yield f"data: {json.dumps({'id': chat_id, 'object': 'chat.completion.chunk', 'model': engine.loaded_model or request.model, 'choices': [{'index': 0, 'delta': args_delta, 'finish_reason': None}]})}\n\n"
                        # Final chunk with finish_reason=tool_calls
                        yield f"data: {json.dumps({'id': chat_id, 'object': 'chat.completion.chunk', 'model': engine.loaded_model or request.model, 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]})}\n\n"

                yield "data: [DONE]\n\n"
            except Exception as e:
                error_chunk = {"error": {"message": str(e), "type": "server_error"}}
                yield f"data: {json.dumps(error_chunk)}\n\n"
                yield "data: [DONE]\n\n"

        return StreamingResponse(generate_stream(), media_type="text/event-stream")
    else:
        # Non-streaming — return full OpenAI-compatible response
        try:
            result = engine.chat(
                messages=messages,
                temperature=request.temperature,
                max_tokens=request.max_tokens,
                top_p=request.top_p,
                top_k=request.top_k,
                repeat_penalty=request.repetition_penalty,
                tools=tools_list,
                tool_choice=request.tool_choice,
            )
            import uuid
            msg = {
                "role": "assistant",
                "content": result["content"],
            }
            if result.get("tool_calls"):
                msg["tool_calls"] = result["tool_calls"]
            return {
                "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
                "object": "chat.completion",
                "model": engine.loaded_model or request.model,
                "choices": [{
                    "index": 0,
                    "message": msg,
                    "finish_reason": result["finish_reason"],
                }],
                "usage": result.get("usage", {}),
            }
        except Exception as e:
            raise HTTPException(status_code=500, detail=str(e))


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main():
    global registry, media_registry

    parser = argparse.ArgumentParser(description="Chitty Workspace Inference Server")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"Port (default: {DEFAULT_PORT})")
    parser.add_argument("--host", type=str, default="127.0.0.1", help="Host (default: 127.0.0.1)")
    parser.add_argument("--models-dir", type=str, action="append", default=[],
                        help="Additional directories to scan for GGUF models (can be repeated)")
    args = parser.parse_args()

    # Build list of model directories
    model_dirs = [MODELS_DIR]
    for d in args.models_dir:
        p = Path(d)
        if p.exists():
            model_dirs.append(p)
            logger.info(f"Added model directory: {p}")
        else:
            logger.warning(f"Model directory does not exist: {d}")

    # Initialize GGUF text model registry
    registry = ModelRegistry(models_dirs=model_dirs)

    # Initialize media model registry (image, video, TTS)
    media_registry = MediaModelRegistry(MEDIA_REGISTRY_FILE)

    logger.info(f"Starting Chitty Workspace Inference Server on {args.host}:{args.port}")
    logger.info(f"Model directories: {[str(d) for d in model_dirs]}")
    logger.info(f"Registry file: {REGISTRY_FILE}")
    logger.info(f"Registered text models: {len(registry.models)}")
    logger.info(f"Registered media models: {len(media_registry.models)}")

    uvicorn.run(app, host=args.host, port=args.port, log_level="info")


if __name__ == "__main__":
    main()
