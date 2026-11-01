"""Koşucunun saf mantığı için birim testleri (canlı masaüstü gerektirmez).

    python3 -m unittest discover -s evals
"""

import json
import tempfile
import unittest
from pathlib import Path

import run_evals as ev


class FillTests(unittest.TestCase):
    def test_nested_templates(self):
        vars = {"target": "Şeftali", "window": "0xabc"}
        got = ev.fill({"args": ["--window", "{window}"], "expect": {"clicked": "{target}"}}, vars)
        self.assertEqual(got, {"args": ["--window", "0xabc"], "expect": {"clicked": "Şeftali"}})

    def test_unknown_and_non_string_kept(self):
        self.assertEqual(ev.fill("{yok}", {}), "{yok}")
        self.assertIs(ev.fill(True, {}), True)


class AnswerTests(unittest.TestCase):
    def test_last_cevap_line_wins(self):
        text = "CEVAP: taslak\nKontrol ettim.\nCEVAP: `QX7-48KD`"
        self.assertEqual(ev.extract_answer(text), "`QX7-48KD`")

    def test_normalized_match_and_alternatives(self):
        ok, _ = ev.check_answer("CEVAP: **sigsegv**", ["SIGSEGV", "SEGV"])
        self.assertTrue(ok)
        ok, detail = ev.check_answer("bilmiyorum", "X")
        self.assertFalse(ok)
        self.assertIn("CEVAP", detail)


class OutJsonTests(unittest.TestCase):
    def test_fields_and_missing_file(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "out.json"
            self.assertFalse(ev.check_out_json(out, {"clicked": "A"})[0])
            out.write_text(json.dumps({"name": "Öykü Işık", "newsletter": True}))
            self.assertTrue(ev.check_out_json(out, {"name": "Öykü Işık", "newsletter": True})[0])
            ok, detail = ev.check_out_json(out, {"newsletter": False})
            self.assertFalse(ok)
            self.assertIn("newsletter", detail)


class StreamTests(unittest.TestCase):
    def test_parse_stream_counts_tools_and_usage(self):
        lines = [
            json.dumps({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "mcp__pc-eye__pc_eye_context"},
                {"type": "text", "text": "bakıyorum"}]}}),
            json.dumps({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "mcp__pc-eye__pc_eye_context"},
                {"type": "tool_use", "name": "mcp__pc-eye__pc_eye_ui"}]}}),
            "bozuk satır",
            json.dumps({"type": "result", "subtype": "success", "result": "CEVAP: tamam",
                        "num_turns": 3, "total_cost_usd": 0.02,
                        "usage": {"input_tokens": 10, "cache_read_input_tokens": 90,
                                  "output_tokens": 5}}),
        ]
        m = ev.parse_stream(lines)
        self.assertEqual(m["tools"], {"pc_eye_context": 2, "pc_eye_ui": 1})
        self.assertEqual((m["tool_calls"], m["turns"], m["input_tokens"]), (3, 3, 100))
        self.assertFalse(m["is_error"])


class ApiErrorTests(unittest.TestCase):
    def test_usage_limit_is_reported_not_scored(self):
        line = json.dumps({"type": "result", "subtype": "success", "is_error": True,
                           "api_error": "usage_limit_reached", "result": "limit"})
        self.assertEqual(ev.parse_stream([line])["api_error"], "usage_limit_reached")


class SummaryTests(unittest.TestCase):
    def test_setup_errors_not_scored(self):
        runs = [
            {"task": "a", "ok": True, "ms": 10},
            {"task": "a", "ok": False, "ms": 30},
            {"task": "a", "ok": False, "ms": 0, "setup_error": True},
        ]
        s = ev.summarize(runs)
        self.assertEqual(s["pass_rate"], 0.5)
        self.assertEqual(s["tasks"][0]["setup_errors"], 1)
        self.assertEqual(s["tasks"][0]["median_ms"], 20)


class TaskFileTests(unittest.TestCase):
    def test_all_tasks_load_and_have_checks(self):
        tasks = ev.load_tasks()
        self.assertGreater(len(tasks), 0)
        ids = [t["id"] for t in tasks]
        self.assertEqual(len(ids), len(set(ids)))
        for t in tasks:
            self.assertIn(t["check"]["type"], ("out_json", "answer", "none", "files"))
            self.assertTrue(t.get("prompt") or t.get("probe"), t["id"])


if __name__ == "__main__":
    unittest.main()


class RedactTest(unittest.TestCase):
    def test_user_titles_masked_scene_titles_kept(self):
        scene = ev.Scene({"title": "Eval Form AB12"})
        out = '{"windows":[{"title": "Eval Form AB12"},{"title": "Banka hesabım — Mozilla Firefox"}]}'
        red = ev.redact_user_titles(out, scene)
        self.assertIn("Eval Form AB12", red)
        self.assertNotIn("Banka", red)


class FilesCheckTest(unittest.TestCase):
    def test_files_check(self):
        with tempfile.TemporaryDirectory() as d:
            a, b = Path(d) / "a.txt", Path(d) / "b.txt"
            b.write_text("merhaba dünya", encoding="utf-8")
            ok, _ = ev.check_files({"exists": [str(b)], "absent": [str(a)], "contains": {str(b): "dünya"}})
            self.assertTrue(ok)
            ok, detail = ev.check_files({"exists": [str(a)]})
            self.assertFalse(ok)
            self.assertIn("a.txt", detail)

    def test_fill_fills_dict_keys(self):
        self.assertEqual(ev.fill({"{p}": "{v}"}, {"p": "/x", "v": "y"}), {"/x": "y"})

