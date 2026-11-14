"""Regression checks for graph snapshots captured during PipeWire changes."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "audio_integration", Path(__file__).with_name("audio-integration.py"))
audio_integration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audio_integration)


class GraphTests(unittest.TestCase):
    def test_snapshot(self):
        graph = [{"id": 1, "type": "PipeWire:Interface:Node", "info": {"state": "running"}}]
        self.assertEqual(audio_integration.parse_graph(json.dumps(graph)), graph)

    def test_removals_updates_and_reused_ids(self):
        batches = [
            [{"id": 9, "info": None}],
            [{"id": 1, "info": {"state": "idle"}}, {"id": 2, "props": {}}],
            [{"id": 1, "info": None}],
            [{"id": 2, "props": None}],
            [{"id": 1, "info": {"state": "running"}}],
        ]
        output = "\n".join(json.dumps(batch) for batch in batches)
        self.assertEqual(audio_integration.parse_graph(output), batches[-1])

    def test_invalid_or_truncated_output_fails(self):
        for output in ["", "[] garbage", '[{"id": 1', '[] [{"id": 2']:
            with self.subTest(output=output), self.assertRaises(ValueError):
                audio_integration.parse_graph(output)


if __name__ == "__main__":
    unittest.main()
