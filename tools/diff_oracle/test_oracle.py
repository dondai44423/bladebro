"""The expanded conformance battery must survive real CLI artifact offloading."""
import json
import pathlib
import tempfile
import unittest

from oracle import parse_bladebro_eval


class EvalParsing(unittest.TestCase):
    @staticmethod
    def envelope(value, **flags):
        return json.dumps(dict(ok=True, is_error=False, text=json.dumps(value), **flags))

    def test_inline_and_legacy_batteries(self):
        battery = {"behavior.query.missing": '{"phase":"reject"}'}
        self.assertEqual(parse_bladebro_eval(self.envelope(json.dumps(battery))), battery)
        self.assertEqual(parse_bladebro_eval('result: ' + json.dumps(json.dumps(battery))), battery)

    def test_full_artifact_preserves_tail_observations(self):
        battery = {str(n): "x" * 200 for n in range(200)}
        battery["last-check"] = "rejected,microtask"
        with tempfile.TemporaryDirectory() as home:
            path = pathlib.Path(home) / "observations.json"
            path.write_text(json.dumps(json.dumps(battery)), encoding="utf-8")
            result = parse_bladebro_eval(self.envelope({"artifact": str(path)}))
        self.assertEqual(result, battery)

    def test_data_envelope(self):
        self.assertEqual(parse_bladebro_eval(self.envelope({"data": {"x": 1}, "note": "full result"})), {"x": 1})

    def test_error_or_nonobject_never_becomes_clean_battery(self):
        for payload in (json.dumps({"ok": False, "text": "{}"}), self.envelope([1, 2])):
            with self.assertRaises(SystemExit):
                parse_bladebro_eval(payload)


if __name__ == "__main__":
    unittest.main()
