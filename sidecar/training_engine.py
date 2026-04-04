"""
Training Engine — LoRA/QLoRA fine-tuning for local models.

Manages training jobs as background threads, with progress reporting
and VRAM coordination (unloads inference/media models before training).

Used by inference_server.py alongside InferenceEngine and MediaEngine.
"""

import base64
import csv
import gc
import io
import json
import logging
import os
import shutil
import threading
import time
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger("chitty-training")

# Default LoRA target modules for common architectures
DEFAULT_TARGET_MODULES = [
    "q_proj", "k_proj", "v_proj", "o_proj",
    "gate_proj", "up_proj", "down_proj",
]


# ---------------------------------------------------------------------------
# Dataset Manager
# ---------------------------------------------------------------------------

class DatasetManager:
    """Manages training datasets in ~/.chitty-workspace/datasets/."""

    def __init__(self, data_dir: Optional[Path] = None):
        self.datasets_dir = (data_dir or self._default_data_dir()) / "datasets"
        self.datasets_dir.mkdir(parents=True, exist_ok=True)

    @staticmethod
    def _default_data_dir() -> Path:
        home = os.environ.get("APPDATA") or os.environ.get("HOME") or str(Path.home())
        return Path(home) / "datavisions" / "chitty-workspace" / "data"

    def upload(self, filename: str, content_bytes: bytes) -> Dict[str, Any]:
        """Save a dataset file and validate its format."""
        safe_name = "".join(c for c in filename if c.isalnum() or c in "._- ").strip()
        if not safe_name:
            safe_name = f"dataset-{int(time.time())}.jsonl"

        path = self.datasets_dir / safe_name
        path.write_bytes(content_bytes)

        try:
            meta = self._validate(path)
            meta["name"] = safe_name
            meta["path"] = str(path)
            meta["size_bytes"] = len(content_bytes)
            logger.info(f"Dataset uploaded: {safe_name} ({meta['row_count']} rows, {meta['format']})")
            return {"success": True, **meta}
        except Exception as e:
            path.unlink(missing_ok=True)
            return {"success": False, "error": str(e)}

    def list_datasets(self) -> List[Dict[str, Any]]:
        """List all available datasets with metadata."""
        results = []
        for f in sorted(self.datasets_dir.iterdir()):
            if f.suffix in (".jsonl", ".csv", ".json"):
                try:
                    meta = self._validate(f)
                    meta["name"] = f.name
                    meta["path"] = str(f)
                    meta["size_bytes"] = f.stat().st_size
                    results.append(meta)
                except Exception as e:
                    results.append({
                        "name": f.name,
                        "path": str(f),
                        "size_bytes": f.stat().st_size,
                        "format": "unknown",
                        "row_count": 0,
                        "error": str(e),
                    })
        return results

    def delete(self, name: str) -> bool:
        """Delete a dataset by name."""
        path = self.datasets_dir / name
        if path.exists() and path.parent == self.datasets_dir:
            path.unlink()
            logger.info(f"Dataset deleted: {name}")
            return True
        return False

    def _validate(self, path: Path) -> Dict[str, Any]:
        """Validate a dataset file and detect its format."""
        suffix = path.suffix.lower()

        if suffix == ".csv":
            return self._validate_csv(path)
        elif suffix in (".jsonl", ".json"):
            return self._validate_jsonl(path)
        else:
            raise ValueError(f"Unsupported file format: {suffix}")

    def _validate_jsonl(self, path: Path) -> Dict[str, Any]:
        """Validate JSONL dataset (Alpaca or Chat format)."""
        rows = []
        with open(path, "r", encoding="utf-8") as f:
            for i, line in enumerate(f):
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError as e:
                    raise ValueError(f"Invalid JSON on line {i+1}: {e}")
                rows.append(obj)
                if i >= 5000:
                    break  # Count cap for validation

        if not rows:
            raise ValueError("Dataset is empty")

        first = rows[0]
        # Detect format
        if "messages" in first and isinstance(first["messages"], list):
            fmt = "chat"
            # Validate messages have role + content
            for msg in first["messages"]:
                if "role" not in msg or "content" not in msg:
                    raise ValueError("Chat format requires 'role' and 'content' in each message")
        elif "instruction" in first or "output" in first:
            fmt = "alpaca"
        elif "text" in first:
            fmt = "text"
        else:
            raise ValueError(
                "Unrecognized format. Expected: "
                "{'instruction','output'} (Alpaca), "
                "{'messages':[...]} (Chat), or "
                "{'text':'...'} (Raw text)"
            )

        # Full row count
        row_count = len(rows)
        if row_count >= 5000:
            # Count remaining lines
            with open(path, "r", encoding="utf-8") as f:
                row_count = sum(1 for line in f if line.strip())

        sample = rows[:3]

        return {
            "format": fmt,
            "row_count": row_count,
            "columns": list(first.keys()),
            "sample": sample,
        }

    def _validate_csv(self, path: Path) -> Dict[str, Any]:
        """Validate CSV dataset."""
        with open(path, "r", encoding="utf-8") as f:
            reader = csv.DictReader(f)
            if not reader.fieldnames:
                raise ValueError("CSV has no headers")

            columns = list(reader.fieldnames)
            has_instruction = "instruction" in columns
            has_output = "output" in columns
            has_text = "text" in columns

            if not (has_instruction or has_text):
                raise ValueError(
                    f"CSV must have 'instruction' or 'text' column. Found: {columns}"
                )

            rows = []
            for i, row in enumerate(reader):
                rows.append(dict(row))
                if i >= 5000:
                    break

        row_count = len(rows)
        if row_count >= 5000:
            with open(path, "r", encoding="utf-8") as f:
                row_count = sum(1 for _ in f) - 1  # Minus header

        return {
            "format": "csv",
            "row_count": row_count,
            "columns": columns,
            "sample": rows[:3],
        }


# ---------------------------------------------------------------------------
# Training Engine
# ---------------------------------------------------------------------------

class TrainingEngine:
    """Manages LoRA/QLoRA training jobs in background threads."""

    def __init__(self, data_dir: Optional[Path] = None):
        self._data_dir = data_dir or DatasetManager._default_data_dir()
        self.adapters_dir = self._data_dir / "adapters"
        self.adapters_dir.mkdir(parents=True, exist_ok=True)

        self.current_job: Optional[Dict[str, Any]] = None
        self.job_history: List[Dict[str, Any]] = []

        self._training_thread: Optional[threading.Thread] = None
        self._stop_event = threading.Event()
        self._progress: Dict[str, Any] = {}
        self._progress_lock = threading.Lock()

        # Load job history from disk
        self._history_path = self._data_dir / "training_history.json"
        self._load_history()

    def _load_history(self):
        """Load job history from disk."""
        if self._history_path.exists():
            try:
                with open(self._history_path, "r") as f:
                    self.job_history = json.load(f)
            except Exception:
                self.job_history = []

    def _save_history(self):
        """Persist job history to disk."""
        try:
            with open(self._history_path, "w") as f:
                json.dump(self.job_history, f, indent=2, default=str)
        except Exception as e:
            logger.error(f"Failed to save training history: {e}")

    def is_running(self) -> bool:
        """Check if a training job is currently running."""
        return (
            self._training_thread is not None
            and self._training_thread.is_alive()
        )

    def start_training(self, config: Dict[str, Any]) -> Dict[str, Any]:
        """Start a new LoRA training job in a background thread."""
        if self.is_running():
            return {"success": False, "error": "A training job is already running"}

        job_id = str(uuid.uuid4())[:8]
        self._stop_event.clear()

        self.current_job = {
            "job_id": job_id,
            "status": "starting",
            "base_model": config.get("base_model", ""),
            "dataset": config.get("dataset", ""),
            "config": config,
            "started_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
            "finished_at": None,
            "error": None,
        }

        with self._progress_lock:
            self._progress = {
                "job_id": job_id,
                "status": "starting",
                "step": 0,
                "total_steps": 0,
                "epoch": 0,
                "total_epochs": config.get("num_epochs", 3),
                "loss": None,
                "learning_rate": None,
                "eta_seconds": None,
                "started_at": self.current_job["started_at"],
            }

        self._training_thread = threading.Thread(
            target=self._training_loop,
            args=(job_id, config),
            daemon=True,
        )
        self._training_thread.start()

        logger.info(f"Training job {job_id} started: {config.get('base_model')}")
        return {"success": True, "job_id": job_id}

    def stop_training(self) -> Dict[str, Any]:
        """Request cancellation of the current training job."""
        if not self.is_running():
            return {"success": False, "error": "No training job is running"}

        self._stop_event.set()
        logger.info("Training stop requested")
        return {"success": True, "message": "Stop signal sent"}

    def get_progress(self) -> Dict[str, Any]:
        """Get current training progress (thread-safe)."""
        with self._progress_lock:
            return dict(self._progress)

    def list_jobs(self) -> List[Dict[str, Any]]:
        """List all training jobs (current + history)."""
        jobs = list(self.job_history)
        if self.current_job:
            jobs.append(self.current_job)
        return jobs

    def list_adapters(self) -> List[Dict[str, Any]]:
        """List all saved adapters."""
        adapters = []
        if not self.adapters_dir.exists():
            return adapters

        for d in sorted(self.adapters_dir.iterdir()):
            if d.is_dir():
                meta_path = d / "training_metadata.json"
                if meta_path.exists():
                    try:
                        with open(meta_path, "r") as f:
                            meta = json.load(f)
                        meta["adapter_path"] = str(d)
                        # Calculate adapter size
                        total_size = sum(
                            f.stat().st_size for f in d.rglob("*") if f.is_file()
                        )
                        meta["size_mb"] = round(total_size / (1024 * 1024), 1)
                        adapters.append(meta)
                    except Exception:
                        adapters.append({
                            "job_id": d.name,
                            "adapter_path": str(d),
                            "error": "Failed to read metadata",
                        })
        return adapters

    def delete_adapter(self, job_id: str) -> Dict[str, Any]:
        """Delete an adapter directory."""
        adapter_dir = self.adapters_dir / job_id
        if not adapter_dir.exists():
            return {"success": False, "error": "Adapter not found"}
        if not adapter_dir.parent == self.adapters_dir:
            return {"success": False, "error": "Invalid adapter path"}

        shutil.rmtree(adapter_dir)
        logger.info(f"Adapter deleted: {job_id}")
        return {"success": True}

    def merge_adapter(self, job_id: str, output_name: Optional[str] = None) -> Dict[str, Any]:
        """Merge a LoRA adapter into the base model and save as safetensors."""
        adapter_dir = self.adapters_dir / job_id
        if not adapter_dir.exists():
            return {"success": False, "error": "Adapter not found"}

        meta_path = adapter_dir / "training_metadata.json"
        if not meta_path.exists():
            return {"success": False, "error": "No training metadata found"}

        try:
            with open(meta_path, "r") as f:
                meta = json.load(f)

            base_model = meta.get("base_model", "")
            if not base_model:
                return {"success": False, "error": "No base model recorded in metadata"}

            import torch
            from transformers import AutoModelForCausalLM, AutoTokenizer
            from peft import PeftModel

            logger.info(f"Merging adapter {job_id} with base model {base_model}")

            # Load base model in fp16
            tokenizer = AutoTokenizer.from_pretrained(base_model)
            model = AutoModelForCausalLM.from_pretrained(
                base_model,
                torch_dtype=torch.float16,
                device_map="auto",
            )

            # Load and merge adapter
            model = PeftModel.from_pretrained(model, str(adapter_dir))
            model = model.merge_and_unload()

            # Save merged model
            if not output_name:
                output_name = f"{Path(base_model).name}-lora-{job_id}"
            merged_dir = self._data_dir / "merged_models" / output_name
            merged_dir.mkdir(parents=True, exist_ok=True)

            model.save_pretrained(merged_dir)
            tokenizer.save_pretrained(merged_dir)

            # Free VRAM
            del model
            gc.collect()
            try:
                torch.cuda.empty_cache()
            except Exception:
                pass

            logger.info(f"Merged model saved to: {merged_dir}")
            return {
                "success": True,
                "output_path": str(merged_dir),
                "output_name": output_name,
            }

        except Exception as e:
            logger.error(f"Merge failed: {e}")
            gc.collect()
            try:
                import torch
                torch.cuda.empty_cache()
            except Exception:
                pass
            return {"success": False, "error": str(e)}

    # -----------------------------------------------------------------------
    # Training loop (runs in background thread)
    # -----------------------------------------------------------------------

    def _training_loop(self, job_id: str, config: Dict[str, Any]):
        """Core training loop — runs in a background thread."""
        start_time = time.time()

        try:
            self._update_progress(status="loading", step=0)

            # Lazy imports — only load when training starts
            import torch
            from transformers import (
                AutoModelForCausalLM,
                AutoTokenizer,
                TrainingArguments,
                TrainerCallback,
                TrainerControl,
                TrainerState,
            )
            from peft import LoraConfig, get_peft_model, prepare_model_for_kbit_training
            from trl import SFTTrainer
            from datasets import Dataset

            # ── Config extraction ──
            base_model = config["base_model"]
            dataset_path = config["dataset_path"]
            quantization = config.get("quantization", "4bit")
            lora_r = config.get("lora_r", 16)
            lora_alpha = config.get("lora_alpha", 32)
            lora_dropout = config.get("lora_dropout", 0.05)
            target_modules = config.get("target_modules") or DEFAULT_TARGET_MODULES
            learning_rate = config.get("learning_rate", 2e-4)
            num_epochs = config.get("num_epochs", 3)
            batch_size = config.get("batch_size", 4)
            grad_accum = config.get("gradient_accumulation_steps", 4)
            max_seq_length = config.get("max_seq_length", 512)
            warmup_ratio = config.get("warmup_ratio", 0.03)

            output_dir = self.adapters_dir / job_id
            output_dir.mkdir(parents=True, exist_ok=True)

            # ── Load dataset ──
            self._update_progress(status="loading_dataset")
            dataset = self._load_dataset(dataset_path)
            logger.info(f"Dataset loaded: {len(dataset)} samples")

            # ── Quantization config ──
            bnb_config = None
            if quantization == "4bit":
                from transformers import BitsAndBytesConfig
                bnb_config = BitsAndBytesConfig(
                    load_in_4bit=True,
                    bnb_4bit_quant_type="nf4",
                    bnb_4bit_compute_dtype=torch.bfloat16,
                    bnb_4bit_use_double_quant=True,
                )
            elif quantization == "8bit":
                from transformers import BitsAndBytesConfig
                bnb_config = BitsAndBytesConfig(load_in_8bit=True)

            # ── Load model + tokenizer ──
            self._update_progress(status="loading_model")
            logger.info(f"Loading base model: {base_model} ({quantization})")

            tokenizer = AutoTokenizer.from_pretrained(base_model, trust_remote_code=True)
            if tokenizer.pad_token is None:
                tokenizer.pad_token = tokenizer.eos_token

            model_kwargs = {
                "device_map": "auto",
                "trust_remote_code": True,
            }
            if bnb_config:
                model_kwargs["quantization_config"] = bnb_config
            else:
                model_kwargs["torch_dtype"] = torch.bfloat16

            model = AutoModelForCausalLM.from_pretrained(base_model, **model_kwargs)

            if quantization in ("4bit", "8bit"):
                model = prepare_model_for_kbit_training(model)

            # ── Apply LoRA ──
            self._update_progress(status="applying_lora")
            lora_config = LoraConfig(
                r=lora_r,
                lora_alpha=lora_alpha,
                lora_dropout=lora_dropout,
                target_modules=target_modules,
                bias="none",
                task_type="CAUSAL_LM",
            )
            model = get_peft_model(model, lora_config)

            trainable_params = sum(p.numel() for p in model.parameters() if p.requires_grad)
            total_params = sum(p.numel() for p in model.parameters())
            logger.info(
                f"LoRA applied: {trainable_params:,} trainable / "
                f"{total_params:,} total ({100*trainable_params/total_params:.2f}%)"
            )

            # ── Calculate total steps ──
            total_steps = (len(dataset) // (batch_size * grad_accum)) * num_epochs
            if total_steps < 1:
                total_steps = num_epochs
            self._update_progress(total_steps=total_steps, trainable_params=trainable_params)

            # ── Progress callback ──
            engine = self

            class ProgressCallback(TrainerCallback):
                def on_log(self, args, state: TrainerState, control: TrainerControl, logs=None, **kwargs):
                    if logs is None:
                        return
                    elapsed = time.time() - start_time
                    step = state.global_step
                    remaining_steps = max(1, total_steps - step)
                    time_per_step = elapsed / max(1, step)
                    eta = remaining_steps * time_per_step

                    engine._update_progress(
                        status="training",
                        step=step,
                        epoch=round(state.epoch or 0, 2),
                        loss=round(logs.get("loss", 0), 4),
                        learning_rate=logs.get("learning_rate"),
                        eta_seconds=round(eta),
                        elapsed_seconds=round(elapsed),
                    )

                    # Check for cancellation
                    if engine._stop_event.is_set():
                        control.should_training_stop = True
                        logger.info("Training cancelled by user")

            # ── Training arguments ──
            use_bf16 = torch.cuda.is_bf16_supported() if torch.cuda.is_available() else False
            training_args = TrainingArguments(
                output_dir=str(output_dir),
                num_train_epochs=num_epochs,
                per_device_train_batch_size=batch_size,
                gradient_accumulation_steps=grad_accum,
                learning_rate=learning_rate,
                warmup_ratio=warmup_ratio,
                fp16=not use_bf16,
                bf16=use_bf16,
                gradient_checkpointing=True,
                logging_steps=1,
                save_strategy="epoch",
                optim="paged_adamw_8bit" if quantization in ("4bit", "8bit") else "adamw_torch",
                max_grad_norm=0.3,
                weight_decay=0.001,
                report_to="none",
            )

            # ── Format dataset for SFTTrainer ──
            formatted_dataset = self._format_for_sft(dataset, tokenizer)

            # ── Train ──
            self._update_progress(status="training", step=0)
            logger.info("Starting training...")

            trainer = SFTTrainer(
                model=model,
                args=training_args,
                train_dataset=formatted_dataset,
                processing_class=tokenizer,
                max_seq_length=max_seq_length,
                callbacks=[ProgressCallback()],
            )

            result = trainer.train()

            # Check if cancelled
            if self._stop_event.is_set():
                status = "cancelled"
                logger.info(f"Training job {job_id} was cancelled")
            else:
                status = "completed"
                logger.info(f"Training job {job_id} completed")

            # Save adapter
            trainer.save_model(str(output_dir))
            tokenizer.save_pretrained(str(output_dir))

            # Save metadata
            meta = {
                "job_id": job_id,
                "base_model": base_model,
                "dataset": config.get("dataset", ""),
                "status": status,
                "lora_r": lora_r,
                "lora_alpha": lora_alpha,
                "num_epochs": num_epochs,
                "learning_rate": learning_rate,
                "batch_size": batch_size,
                "max_seq_length": max_seq_length,
                "quantization": quantization,
                "trainable_params": trainable_params,
                "final_loss": round(result.training_loss, 4) if hasattr(result, "training_loss") else None,
                "total_steps": result.global_step if hasattr(result, "global_step") else 0,
                "duration_seconds": round(time.time() - start_time),
                "started_at": self.current_job["started_at"],
                "finished_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
            }
            with open(output_dir / "training_metadata.json", "w") as f:
                json.dump(meta, f, indent=2)

            self._update_progress(
                status=status,
                step=meta["total_steps"],
                final_loss=meta["final_loss"],
                duration_seconds=meta["duration_seconds"],
            )

            # Move to history
            if self.current_job:
                self.current_job["status"] = status
                self.current_job["finished_at"] = meta["finished_at"]
                self.job_history.append(self.current_job)
                self._save_history()
                self.current_job = None

        except Exception as e:
            logger.error(f"Training failed: {e}", exc_info=True)
            self._update_progress(status="failed", error=str(e))
            if self.current_job:
                self.current_job["status"] = "failed"
                self.current_job["error"] = str(e)
                self.current_job["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
                self.job_history.append(self.current_job)
                self._save_history()
                self.current_job = None

        finally:
            # Free VRAM
            gc.collect()
            try:
                import torch
                torch.cuda.empty_cache()
            except Exception:
                pass

    def _update_progress(self, **kwargs):
        """Thread-safe progress update."""
        with self._progress_lock:
            self._progress.update(kwargs)

    def _load_dataset(self, dataset_path: str) -> 'Dataset':
        """Load a dataset file into a HuggingFace Dataset."""
        from datasets import Dataset

        path = Path(dataset_path)
        if not path.exists():
            raise FileNotFoundError(f"Dataset not found: {dataset_path}")

        if path.suffix == ".csv":
            import csv as csv_mod
            with open(path, "r", encoding="utf-8") as f:
                reader = csv_mod.DictReader(f)
                rows = [dict(r) for r in reader]
        else:
            # JSONL
            rows = []
            with open(path, "r", encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if line:
                        rows.append(json.loads(line))

        return Dataset.from_list(rows)

    def _format_for_sft(self, dataset: 'Dataset', tokenizer) -> 'Dataset':
        """Format dataset into 'text' column for SFTTrainer."""
        columns = dataset.column_names

        if "text" in columns:
            # Already in text format
            return dataset

        if "messages" in columns:
            # Chat format — apply chat template
            def format_chat(example):
                try:
                    text = tokenizer.apply_chat_template(
                        example["messages"], tokenize=False, add_generation_prompt=False
                    )
                except Exception:
                    # Fallback: simple concat
                    parts = []
                    for msg in example["messages"]:
                        role = msg.get("role", "user")
                        content = msg.get("content", "")
                        parts.append(f"<|{role}|>\n{content}")
                    text = "\n".join(parts)
                return {"text": text}

            return dataset.map(format_chat, remove_columns=columns)

        if "instruction" in columns:
            # Alpaca format
            def format_alpaca(example):
                instruction = example.get("instruction", "")
                inp = example.get("input", "")
                output = example.get("output", "")

                if inp:
                    text = (
                        f"### Instruction:\n{instruction}\n\n"
                        f"### Input:\n{inp}\n\n"
                        f"### Response:\n{output}"
                    )
                else:
                    text = (
                        f"### Instruction:\n{instruction}\n\n"
                        f"### Response:\n{output}"
                    )
                return {"text": text}

            return dataset.map(format_alpaca, remove_columns=columns)

        raise ValueError(f"Cannot format dataset with columns: {columns}")
