#!/usr/bin/env python3
"""Linux worker for Memorious desktop push-to-talk.

This process owns microphone/key handling only.  It never opens the journal:
completed notes are delivered to the background daemon's owner-only Unix
socket as ordinary `capture_text` RPCs, preserving the WAV's capture time.
"""

import argparse
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import threading
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

import evdev
from evdev import ecodes
import requests

RUNNING = True
MIN_WAV_BYTES = 20_000
POLL_SECONDS = 10
CLEANUP_PROMPT = (
    "Clean this personal-journal voice transcript. Fix punctuation and remove "
    "filler and false starts, but do not summarize or add wording. Keep the original "
    "language; never translate. Return only text."
)


def load(path):
    return json.loads(path.read_text())


def atomic_json(path, value):
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(value))
    os.chmod(tmp, 0o600)
    tmp.replace(path)


class Worker:
    def __init__(self, data, socket_path):
        self.data = data
        self.socket_path = socket_path
        config = data / ".memorious-ptt.json"
        self.config = load(config) if config.exists() else {"provider":"local", "key":"KEY_RIGHTCTRL"}
        self.spool = data / "ptt-spool"
        self.spool.mkdir(mode=0o700, exist_ok=True)
        os.chmod(self.spool, 0o700)
        self.status_path = data / ".memorious-ptt-status.json"
        self.recording = None
        self.recording_path = data / ".memorious-ptt-recording.wav"
        self.lock = threading.Lock()
        self.spool_ready = threading.Event()
        self.stop_event = threading.Event()
        self.status_lock = threading.Lock()
        self.processing = False

    def status(self, state, error=None):
        if self.recording is not None:
            state = "recording"
        elif self.processing and state in ("waiting", "idle"):
            state = "transcribing"
        value = {"pid": os.getpid(), "state": state, "queued": len(list(self.spool.glob("*.wav")))}
        if error:
            value["error"] = str(error)
        with self.status_lock:
            atomic_json(self.status_path, value)

    def start(self):
        with self.lock:
            if self.recording:
                return
            self.recording_path.unlink(missing_ok=True)
            self.recording = subprocess.Popen(
                ["pw-record", "--rate", "16000", "--channels", "1", "--format", "s16", str(self.recording_path)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            self.status("recording")

    def stop(self):
        with self.lock:
            if not self.recording:
                return
            self.recording.send_signal(signal.SIGINT)
            try:
                self.recording.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.recording.kill()
            self.recording = None
            if self.recording_path.exists() and self.recording_path.stat().st_size >= MIN_WAV_BYTES:
                stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
                self.recording_path.replace(self.spool / f"{stamp}.wav")
            else:
                self.recording_path.unlink(missing_ok=True)
            self.status("idle")
            self.spool_ready.set()

    def request_stop(self):
        self.stop_event.set()
        self.spool_ready.set()
        # Signals may arrive while this thread owns the recorder mutex.
        thread = threading.Thread(target=self.stop)
        thread.start()
        return thread

    def local_transcribe(self, wav):
        from transcription import transcribe_local
        return transcribe_local(wav, self.config.get("language", "auto"))

    def remote_transcribe(self, wav):
        data = {"model": self.config["stt_model"]}
        language = self.config.get("language", "auto")
        if language != "auto":
            data["language"] = language
        with wav.open("rb") as audio:
            response = requests.post(
                "https://api.openai.com/v1/audio/transcriptions",
                headers={"Authorization": f"Bearer {self.config['api_key']}"},
                files={"file": (wav.name, audio, "audio/wav")},
                data=data, timeout=120,
            )
        response.raise_for_status()
        return response.json()["text"].strip()

    def clean(self, text):
        try:
            response = requests.post(
                "https://api.openai.com/v1/chat/completions",
                headers={"Authorization": f"Bearer {self.config['api_key']}"},
                json={"model": self.config["cleanup_model"], "messages": [
                    {"role": "system", "content": CLEANUP_PROMPT},
                    {"role": "user", "content": text},
                ]}, timeout=60,
            )
            response.raise_for_status()
            return response.json()["choices"][0]["message"]["content"].strip() or text
        except Exception:
            return text

    def submit(self, text, recorded_at):
        request = json.dumps({"id": 1, "method": "capture_text", "params": {
            "text": text, "recorded_at": recorded_at,
        }}).encode() + b"\n"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(30)
            client.connect(str(self.socket_path))
            client.sendall(request)
            reply = json.loads(client.makefile("rb").readline())
        if reply.get("error"):
            raise RuntimeError(reply["error"])

    def local_selected(self):
        provider = self.config.get("provider", "remote")
        if provider not in ("local", "remote"):
            raise RuntimeError("Select local or remote transcription in Memorious settings")
        return provider == "local"

    def transcribe_path(self, path):
        local = self.local_selected()
        def transcribe(wav):
            text = self.local_transcribe(wav) if local else self.remote_transcribe(wav)
            return text if local else self.clean(text)
        if path.suffix.lower() == ".wav":
            return transcribe(path)
        with tempfile.TemporaryDirectory(prefix="memorious-decode-") as directory:
            wav = Path(directory) / "voice.wav"
            try:
                result = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-i", str(path),
                    "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", str(wav)],
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=120)
                if result.returncode:
                    raise RuntimeError("Could not decode the recording for transcription")
                return transcribe(wav)
            finally:
                if wav.exists():
                    with wav.open("r+b") as file:
                        remaining = wav.stat().st_size
                        while remaining:
                            block = min(remaining, 65536)
                            file.write(b"\0" * block)
                            remaining -= block

    def process(self, wav):
        self.processing = True
        try:
            self.status("transcribing")
            recorded_at = int(wav.stat().st_mtime * 1000)
            self.config = load(self.data / ".memorious-ptt.json")
            if not self.config.get("enabled", True):
                raise RuntimeError("Push-to-talk is paused; capture retained for later")
            local = self.local_selected()
            text = self.local_transcribe(wav) if local else self.remote_transcribe(wav)
            if text:
                self.submit(text if local else self.clean(text), recorded_at)
            wav.unlink()
        finally:
            self.processing = False
            self.status("idle")

    def spool_loop(self):
        while RUNNING:
            self.spool_ready.clear()
            for wav in sorted(self.spool.glob("*.wav")):
                if not RUNNING:
                    break
                try:
                    self.process(wav)
                except Exception as error:
                    self.status("idle", error)
                    break  # retain the ordered spool; retry next cycle
            self.spool_ready.wait(POLL_SECONDS)

    def control_loop(self):
        """Owner-only local buttons share the same recorder as the global key."""
        path = self.data / ".memorious-ptt-control.sock"
        path.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(path))
            os.chmod(path, 0o600)
            server.listen(4)
            server.settimeout(.5)
            try:
                while RUNNING:
                    try:
                        client, _ = server.accept()
                    except socket.timeout:
                        continue
                    with client:
                        client.settimeout(2)
                        try:
                            command = client.recv(64).decode().strip()
                            if command == "start":
                                self.start()
                            elif command == "stop":
                                self.stop()
                            elif command == "toggle":
                                if self.recording:
                                    self.stop()
                                else:
                                    self.start()
                            else:
                                raise ValueError("unknown recording command")
                            client.sendall(b"ok\n")
                        except Exception as error:
                            self.status("failed", error)
                            try:
                                client.sendall((str(error) + "\n").encode())
                            except OSError:
                                pass
            finally:
                path.unlink(missing_ok=True)

    def input_loop(self):
        key = getattr(ecodes, self.config["key"], None)
        if key is None:
            raise RuntimeError(f"unknown Linux evdev key {self.config['key']}")
        while RUNNING:
            devices = []
            for path in evdev.list_devices():
                try:
                    device = evdev.InputDevice(path)
                    if key in device.capabilities().get(ecodes.EV_KEY, []):
                        devices.append(device)
                    else:
                        device.close()
                except (PermissionError, OSError):
                    pass
            if not devices:
                if not self.recording:
                    self.status("waiting", "Global key unavailable; use the toolbar recording button")
                self.stop_event.wait(10)
                continue
            selected = selectors.DefaultSelector()
            for device in devices:
                selected.register(device, selectors.EVENT_READ)
            try:
                while RUNNING:
                    for selected_key, _ in selected.select(timeout=1):
                        for event in selected_key.fileobj.read():
                            if event.type == ecodes.EV_KEY and event.code == key:
                                if event.value == 1:
                                    self.start()
                                elif event.value == 0:
                                    self.stop()
            finally:
                for device in devices:
                    selected.unregister(device)
                    device.close()


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--data-dir", type=Path)
    parser.add_argument("--socket", type=Path)
    parser.add_argument("--model-status", action="store_true")
    parser.add_argument("--download-model", action="store_true")
    parser.add_argument("--transcribe-file", type=Path)
    args = parser.parse_args()
    if args.model_status or args.download_model:
        from transcription import model_info, download_model
        try:
            print(json.dumps(download_model() if args.download_model else model_info()))
            return 0
        except Exception as error:
            print(str(error), file=sys.stderr)
            return 1
    if args.data_dir is None or (args.socket is None and args.transcribe_file is None):
        parser.error("recording requires --data-dir and --socket")
    worker = Worker(args.data_dir, args.socket)
    if args.transcribe_file is not None:
        print(json.dumps({"text":worker.transcribe_path(args.transcribe_file)}))
        return 0

    def stop(*_):
        global RUNNING
        RUNNING = False
        worker.request_stop()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    worker.status("idle")
    threading.Thread(target=worker.spool_loop, daemon=True).start()
    threading.Thread(target=worker.control_loop, daemon=True).start()
    try:
        worker.input_loop()
    except Exception as error:
        worker.status("failed", error)
        return 1
    finally:
        worker.stop()
        worker.status_path.unlink(missing_ok=True)
        (worker.data / ".memorious-ptt-control.sock").unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
