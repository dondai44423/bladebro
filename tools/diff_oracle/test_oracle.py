"""The expanded conformance battery must survive real CLI artifact offloading."""
import json
import os
import pathlib
import shutil
import tempfile
import unittest

from oracle import chrome_candidates, parse_bladebro_eval


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


class StockBrowserResolution(unittest.TestCase):
    """A distro's /usr/bin/chromium can be a snap stub that dies before its
    debug port opens; the baseline must prefer a real browser binary."""

    def test_wrapper_stub_is_demoted_below_a_real_binary(self):
        with tempfile.TemporaryDirectory() as d:
            real = os.path.join(d, "google-chrome-stable")
            shutil.copy("/bin/true", real)
            stub = os.path.join(d, "chromium")
            with open(stub, "w", encoding="utf-8") as f:
                f.write("#!/bin/sh\nexit 1\n")
            os.chmod(stub, 0o700)
            previous = os.environ["PATH"]
            os.environ["PATH"] = d
            os.environ.pop("CHROME_PATH", None)
            try:
                candidates = chrome_candidates(None)
            finally:
                os.environ["PATH"] = previous
            self.assertEqual(candidates[0], real)
            self.assertEqual(candidates[-1], stub)

    def test_explicit_path_stays_first_among_real_binaries(self):
        with tempfile.TemporaryDirectory() as d:
            explicit = os.path.join(d, "custom-chromium")
            shutil.copy("/bin/true", explicit)
            previous = os.environ["PATH"]
            os.environ["PATH"] = d
            os.environ.pop("CHROME_PATH", None)
            try:
                self.assertEqual(chrome_candidates(explicit)[0], explicit)
            finally:
                os.environ["PATH"] = previous


if __name__ == "__main__":
    unittest.main()
