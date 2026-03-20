"""
Chitty Workspace Inference Server — Python sidecar for local model inference.

Managed by the Chitty Workspace Rust binary as a child process.
Provides a REST API for:
  - GGUF model loading and chat completions via llama-cpp-python
  - Image generation via diffusers (SDXL, Flux, Stable Diffusion) — future
  - Audio transcription/synthesis — future

Usage:
    python inference_server.py --port 8766
    python inference_server.py --port 8766 --models-dir "C:/LLM Models"
"""

import argparse
import base64
import gc
import io
import json
import logging
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from fastapi import FastAPI, HTTPException
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field
import uvicorn

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_PORT = 8766
DATA_DIR = Path.home() / ".chitty-workspace"
MODELS_DIR = DATA_DIR / "models"
REGISTRY_FILE = DATA_DIR / "hf_models.json"

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
    context_length: int = Field(4096, description="Context window size")

class ChatMessage(BaseModel):
    role: str
    content: str

class ChatCompletionRequest(BaseModel):
    model: str
    messages: List[ChatMessage]
    temperature: float = 0.7
    max_tokens: int = 2048
    top_p: float = 1.0
    stream: bool = False

class ModelInfo(BaseModel):
    name: str
    path: str
    size_bytes: int
    size_gb: float
    quantization: Optional[str] = None
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
             gpu_layers: int = -1, context_length: int = 4096):
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

        self.llm = self._llama_cpp.Llama(
            model_path=model_path,
            n_ctx=context_length,
            n_gpu_layers=gpu_layers,
            verbose=False,
        )

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

    def chat(self, messages: List[Dict[str, str]],
             temperature: float = 0.7, max_tokens: int = 2048,
             top_p: float = 1.0) -> Dict[str, Any]:
        """Run chat completion on the loaded model."""
        if self.llm is None:
            raise RuntimeError("No model loaded. Call /models/load first.")

        start = time.time()

        response = self.llm.create_chat_completion(
            messages=messages,
            temperature=temperature,
            max_tokens=max_tokens,
            top_p=top_p,
        )

        elapsed = time.time() - start

        # Extract content from response
        choice = response.get("choices", [{}])[0]
        content = choice.get("message", {}).get("content", "")
        finish_reason = choice.get("finish_reason", "stop")

        # Usage stats
        usage = response.get("usage", {})

        return {
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


@app.get("/health")
async def health():
    """Health check — reports sidecar status."""
    return {
        "status": "ok",
        "loaded_model": engine.loaded_model,
        "models_registered": len(registry.models) if registry else 0,
        "vram_free_mb": get_gpu_free_vram_mb(),
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


@app.post("/chat/completions")
async def chat_completions(request: ChatCompletionRequest):
    """OpenAI-compatible chat completion endpoint."""
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

    messages = [{"role": m.role, "content": m.content} for m in request.messages]

    try:
        result = engine.chat(
            messages=messages,
            temperature=request.temperature,
            max_tokens=request.max_tokens,
            top_p=request.top_p,
        )
        return result
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main():
    global registry

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

    # Initialize registry with all model directories
    registry = ModelRegistry(models_dirs=model_dirs)

    logger.info(f"Starting Chitty Workspace Inference Server on {args.host}:{args.port}")
    logger.info(f"Model directories: {[str(d) for d in model_dirs]}")
    logger.info(f"Registry file: {REGISTRY_FILE}")
    logger.info(f"Registered models: {len(registry.models)}")

    uvicorn.run(app, host=args.host, port=args.port, log_level="info")


if __name__ == "__main__":
    main()
