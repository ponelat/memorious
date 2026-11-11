import sys
import tempfile
import unittest
import hashlib
import os
import types
from unittest.mock import patch, MagicMock
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import transcription

class LocalModelFiles(unittest.TestCase):
    def test_limit_covers_every_downloaded_component_and_is_strictly_below_500mb(self):
        self.assertEqual(transcription.model_size([{"size":200_000_000},{"size":99_000_000}]),299_000_000)
        for files in ([{"size":500_000_000}], [{"size":300_000_000},{"size":200_000_000}], [{"size":None}]):
            with self.assertRaises(ValueError): transcription.model_size(files)

    def test_partial_or_wrong_size_models_are_never_reported_ready(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); files=[{"name":"encoder.ort","size":3}]
            (root/'encoder.ort.part').write_bytes(b'123')
            self.assertFalse(transcription.files_ready(root, files))
            (root/'encoder.ort').write_bytes(b'12')
            self.assertFalse(transcription.files_ready(root, files))
            (root/'encoder.ort').write_bytes(b'123')
            self.assertTrue(transcription.files_ready(root, files))

class ModelDownloadIntegrity(unittest.TestCase):
    def response(self, payload, status=200, headers=None):
        response=MagicMock()
        response.__enter__.return_value=response
        response.status_code=status
        response.headers=headers or {}
        response.iter_content.return_value=[payload]
        return response

    def test_bad_hash_never_installs_or_reports_a_ready_model(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"MEMORIOUS_MODEL_DIR":directory}), patch.multiple(transcription, MODEL_BYTES=3, MODEL_SHA256=hashlib.sha256(b"yes").hexdigest()):
            requests=types.SimpleNamespace(get=MagicMock(return_value=self.response(b"bad")))
            with patch.dict(sys.modules, {"requests":requests}):
                with self.assertRaisesRegex(RuntimeError,"checksum"): transcription.download_model()
            self.assertFalse(transcription.model_info()["ready"])
            self.assertFalse((Path(directory)/"whisper-small"/transcription.MODEL_FILE).exists())

    def test_download_resumes_and_verifies_the_complete_file(self):
        payload=b"abcdefgh"
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"MEMORIOUS_MODEL_DIR":directory}), patch.multiple(transcription, MODEL_BYTES=len(payload), MODEL_SHA256=hashlib.sha256(payload).hexdigest()):
            root=Path(directory)/"whisper-small"; root.mkdir()
            target=root/transcription.MODEL_FILE
            target.with_suffix(".bin.part").write_bytes(payload[:3])
            requests=types.SimpleNamespace(get=MagicMock(return_value=self.response(payload[3:],206,{"Content-Range":"bytes 3-7/8"})))
            with patch.dict(sys.modules, {"requests":requests}): result=transcription.download_model()
            self.assertEqual(requests.get.call_args.kwargs["headers"],{"Range":"bytes=3-"})
            self.assertEqual(target.read_bytes(),payload)
            self.assertTrue(result["ready"])

if __name__ == '__main__': unittest.main()
