import importlib.util
import json
import socket
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from unittest.mock import Mock

sys.modules.setdefault("evdev", types.SimpleNamespace(ecodes=types.SimpleNamespace()))
sys.modules.setdefault("requests", types.SimpleNamespace())
spec = importlib.util.spec_from_file_location("ptt_worker", Path(__file__).with_name("memorious-ptt.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class ToolbarControl(unittest.TestCase):
    def test_owner_only_toolbar_controls_the_existing_capture_worker(self):
        with tempfile.TemporaryDirectory() as temporary:
            data = Path(temporary)
            (data / ".memorious-ptt.json").write_text(json.dumps({"key":"KEY_RIGHTCTRL"}))
            worker = module.Worker(data, data / "daemon.sock")
            worker.start = Mock()
            worker.stop = Mock()
            module.RUNNING = True
            thread = threading.Thread(target=worker.control_loop)
            thread.start()
            path = data / ".memorious-ptt-control.sock"
            try:
                for _ in range(100):
                    if path.exists(): break
                    time.sleep(.01)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                for command in ("start", "stop"):
                    with socket.socket(socket.AF_UNIX) as client:
                        client.connect(str(path))
                        client.sendall((command + "\n").encode())
                        self.assertEqual(client.recv(100), b"ok\n")
                worker.start.assert_called_once()
                worker.stop.assert_called_once()
            finally:
                module.RUNNING = False
                thread.join(3)
            self.assertFalse(thread.is_alive())
            self.assertFalse(path.exists())

class TranscriptionProviders(unittest.TestCase):
    def worker(self, directory, provider):
        data = Path(directory)
        (data / ".memorious-ptt.json").write_text(json.dumps({"provider":provider,"api_key":"secret","key":"KEY_RIGHTCTRL"}))
        return module.Worker(data, data / "daemon.sock")

    def test_local_processing_never_calls_remote_transcription_or_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker = self.worker(temporary, "local")
            wav = worker.spool / "capture.wav"
            wav.write_bytes(b"audio")
            worker.local_transcribe = Mock(return_value="A private local thought.")
            worker.clean = Mock(side_effect=AssertionError("local text must not leave the device"))
            worker.submit = Mock()
            worker.process(wav)
            worker.local_transcribe.assert_called_once_with(wav)
            worker.submit.assert_called_once()
            self.assertEqual(worker.submit.call_args.args[0], "A private local thought.")
            self.assertFalse(wav.exists())

    def test_releasing_ptt_wakes_transcription_without_waiting_for_retry_timer(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker = self.worker(temporary, "local")
            worker.recording = Mock()
            worker.recording_path.write_bytes(b"audio" * 5000)
            worker.stop()
            self.assertTrue(worker.spool_ready.is_set())
            self.assertEqual(len(list(worker.spool.glob("*.wav"))), 1)

    def test_failed_local_model_keeps_capture_and_never_falls_back_to_api(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker = self.worker(temporary, "local")
            wav = worker.spool / "capture.wav"
            wav.write_bytes(b"audio")
            worker.local_transcribe = Mock(side_effect=RuntimeError("model missing"))
            worker.remote_transcribe = Mock(side_effect=AssertionError("no automatic cloud fallback"))
            with self.assertRaisesRegex(RuntimeError, "model missing"): worker.process(wav)
            self.assertTrue(wav.exists())
            worker.remote_transcribe.assert_not_called()

    def test_transcribing_status_is_not_overwritten_by_keyboard_waiting(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker=self.worker(temporary,"local")
            worker.processing=True
            worker.status("waiting", "No global keyboard access")
            self.assertEqual(json.loads(worker.status_path.read_text())["state"],"transcribing")

    def test_signal_stop_does_not_deadlock_the_recording_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker=self.worker(temporary,"local")
            worker.recording=Mock()
            worker.recording_path.write_bytes(b"audio"*5000)
            with worker.lock:
                thread=worker.request_stop()
                self.assertTrue(worker.stop_event.is_set())
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertIsNone(worker.recording)
            self.assertEqual(len(list(worker.spool.glob("*.wav"))),1)

    def test_remote_language_hint_is_sent_without_changing_the_model(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as temporary:
            worker = self.worker(temporary, "remote")
            worker.config.update(language="af", stt_model="gpt-4o-mini-transcribe")
            wav = worker.spool / "sample.wav"; wav.write_bytes(b"audio")
            response = Mock(); response.json.return_value={"text":"Goeie more"}
            with patch.object(module.requests, "post", return_value=response, create=True) as post:
                worker.remote_transcribe(wav)
                self.assertEqual(post.call_args.kwargs["data"]["language"], "af")
                self.assertEqual(post.call_args.kwargs["data"]["model"], "gpt-4o-mini-transcribe")

    def test_remote_selection_uses_api_transcription_and_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            worker = self.worker(temporary, "remote")
            wav = worker.spool / "capture.wav"
            wav.write_bytes(b"audio")
            worker.remote_transcribe = Mock(return_value="remote transcript")
            worker.clean = Mock(return_value="Clean remote transcript.")
            worker.submit = Mock()
            worker.process(wav)
            worker.remote_transcribe.assert_called_once_with(wav)
            worker.clean.assert_called_once_with("remote transcript")
            self.assertEqual(worker.submit.call_args.args[0], "Clean remote transcript.")

if __name__ == "__main__": unittest.main()
