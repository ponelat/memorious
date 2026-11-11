"""One local transcription engine for the desktop and its capture worker.

Model downloads are explicit. Inference only loads verified local files and never
falls back to a network service. The journal engine remains in Rust.
"""
import hashlib
import subprocess
import tempfile
import fcntl
import json
import os
import threading
from pathlib import Path

MODEL_NAME = "Whisper Small (multilingual)"
MODEL_FILE = "ggml-small.bin"
MODEL_BYTES = 487_601_967
MODEL_SHA256 = "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"
MODEL_URL = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/" + MODEL_FILE
MAX_MODEL_BYTES = 500_000_000
_engine_lock = threading.Lock()


def cache_root():
    override = os.environ.get("MEMORIOUS_MODEL_DIR")
    if override:
        return Path(override)
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "memorious" / "transcription"


def model_size(files):
    sizes = [file.get("size") for file in files]
    if not sizes or any(not isinstance(size, int) or size <= 0 for size in sizes):
        raise ValueError("Model manifest must declare every component’s download size")
    total = sum(sizes)
    if total >= MAX_MODEL_BYTES:
        raise ValueError("Local model download must be smaller than 500 MB")
    return total


def files_ready(root, files):
    return all((root / file["name"]).is_file() and (root / file["name"]).stat().st_size == file["size"] for file in files)


def model_spec():
    files = [{"name": MODEL_FILE, "size": MODEL_BYTES}]
    size = model_size(files)
    return None, files, cache_root() / "whisper-small", size


def _write_progress(value):
    directory = cache_root()
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = directory / "download.json.tmp"
    temporary.write_text(json.dumps(dict(value, pid=os.getpid(), model=MODEL_FILE)))
    os.chmod(temporary, 0o600)
    temporary.replace(directory / "download.json")


def model_info():
    _, files, root, size = model_spec()
    ready = files_ready(root, files)
    try:
        progress = json.loads((cache_root() / "download.json").read_text())
    except (OSError, ValueError):
        progress = {}
    if progress.get("model") != MODEL_FILE:
        progress = {}
    if progress.get("state") == "downloading":
        try:
            os.kill(progress["pid"], 0)
        except (OSError, KeyError):
            progress = {"state": "failed", "error": "Download interrupted; retry to resume"}
    return dict(progress, name=MODEL_NAME, bytes=size, ready=ready,
                state="ready" if ready else progress.get("state", "missing"))


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_model():
    import requests
    _, files, root, size = model_spec()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (cache_root() / "download.lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return model_info()
        target = root / MODEL_FILE
        if files_ready(root, files) and _sha256(target) == MODEL_SHA256:
            _write_progress({"state": "ready", "fraction": 1})
            return model_info()
        partial = target.with_suffix(".bin.part")
        offset = partial.stat().st_size if partial.exists() else 0
        if offset >= size:
            offset = 0
        try:
            headers = {"Range": f"bytes={offset}-"} if offset else {}
            _write_progress({"state":"downloading", "fraction":offset / size})
            with requests.get(MODEL_URL, headers=headers, stream=True, timeout=(15, 60)) as response:
                response.raise_for_status()
                if offset and response.status_code == 206:
                    if not response.headers.get("Content-Range", "").startswith(f"bytes {offset}-"):
                        raise RuntimeError("Model server returned an invalid resume offset")
                else:
                    offset = 0
                with partial.open("ab" if offset else "wb") as output:
                    for chunk in response.iter_content(1024 * 1024):
                        if not chunk: continue
                        offset += len(chunk)
                        if offset > size:
                            raise RuntimeError("Downloaded model exceeds its declared size")
                        output.write(chunk)
                        _write_progress({"state":"downloading", "fraction":offset / size})
            if offset != size or _sha256(partial) != MODEL_SHA256:
                partial.unlink(missing_ok=True)
                raise RuntimeError("Model checksum verification failed; please retry")
            partial.replace(target)
            _write_progress({"state": "ready", "fraction": 1})
        except Exception as error:
            _write_progress({"state": "failed", "error": str(error)})
            raise
    return model_info()


def transcribe_local(wav, language="auto"):
    _, files, root, _ = model_spec()
    if not files_ready(root, files):
        raise RuntimeError("Download the local model in Memorious transcription settings first")
    with _engine_lock, tempfile.TemporaryDirectory(prefix="memorious-transcript-") as directory:
        output = Path(directory) / "transcript"
        result = subprocess.run([
            "whisper-cli", "--model", str(root / MODEL_FILE), "--file", str(wav),
            "--language", language, "--no-timestamps", "--no-prints",
            "--threads", str(min(8, os.cpu_count() or 2)),
            "--output-txt", "--output-file", str(output),
        ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=300)
        if result.returncode:
            raise RuntimeError("Local transcription failed: " + result.stderr.decode(errors="replace")[-1200:])
        transcript = output.with_suffix(".txt")
        try:
            return transcript.read_text().strip()
        finally:
            if transcript.exists():
                with transcript.open("r+b") as file:
                    file.write(b"\0" * transcript.stat().st_size)
