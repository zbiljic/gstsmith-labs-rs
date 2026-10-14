"""Offline checks for asset validation and image-shape specialization."""

import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import onnx
from onnx import TensorProto, helper

from prepare_segment import prepare, specialize, verify


class PrepareSegmentTests(unittest.TestCase):
    def test_specialization_preserves_source_and_rejects_incompatible_shapes(self):
        with tempfile.TemporaryDirectory() as directory:
            source, output = Path(directory) / "source.onnx", Path(directory) / "fixed.onnx"
            graph = helper.make_graph(
                [helper.make_node("Identity", ["image"], ["output"])], "image",
                [helper.make_tensor_value_info("image", TensorProto.FLOAT, ["batch", 3, 4, 4])],
                [helper.make_tensor_value_info("output", TensorProto.FLOAT, ["batch", 3, 4, 4])],
            )
            model = helper.make_model(graph, ir_version=9, opset_imports=[helper.make_opsetid("", 18)])
            onnx.save(model, source)
            original = source.read_bytes()
            specialize(source, output, 4)
            fixed = onnx.load(output)
            self.assertEqual([d.dim_value for d in fixed.graph.input[0].type.tensor_type.shape.dim], [1, 3, 4, 4])
            self.assertEqual(source.read_bytes(), original)
            self.assertEqual(fixed.graph.node, model.graph.node)
            self.assertEqual(fixed.graph.output, model.graph.output)
            with self.assertRaisesRegex(ValueError, "incompatible"):
                specialize(source, output, 8)
            verify(source, hashlib.sha256(original).hexdigest())
            with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                verify(source, "0" * 64)

    def test_failed_download_does_not_publish_a_file(self):
        with tempfile.TemporaryDirectory() as directory:
            def corrupt_download(command, **_kwargs):
                Path(command[-1]).write_bytes(b"corrupt")
            with patch("prepare_segment.subprocess.run", side_effect=corrupt_download):
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    prepare("fastsam", Path(directory))
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_existing_mismatch_is_preserved_without_downloading(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "FastSAM-s.onnx"
            source.write_bytes(b"existing")
            with patch("prepare_segment.subprocess.run") as download:
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    prepare("fastsam", Path(directory))
                download.assert_not_called()
            self.assertEqual(source.read_bytes(), b"existing")


if __name__ == "__main__":
    unittest.main()
