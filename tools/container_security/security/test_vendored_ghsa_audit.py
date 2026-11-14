#!/usr/bin/env python3
"""Tests for vendored_ghsa_audit.py（勧告の版の範囲を読み違えて該当を見逃さないことを確かめる。B-400）。

使い方: python3 tools/container_security/security/test_vendored_ghsa_audit.py
"""

import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("vendored_ghsa_audit", HERE / "vendored_ghsa_audit.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class InRangeTests(unittest.TestCase):
    def test_single_bounds(self):
        self.assertTrue(audit.in_range("0.15.0", ">= 0.15.0"))
        self.assertFalse(audit.in_range("0.14.9", ">= 0.15.0"))
        self.assertTrue(audit.in_range("0.24.3", "< 0.24.4"))
        self.assertFalse(audit.in_range("0.24.4", "< 0.24.4"))
        self.assertTrue(audit.in_range("0.19.0", "<0.19.1"))
        self.assertTrue(audit.in_range("0.8.0", "> 0.7.0"))
        self.assertFalse(audit.in_range("0.7.0", "> 0.7.0"))

    def test_open_ended_and_exact(self):
        self.assertTrue(audit.in_range("0.24.9", "0.15.0+"))
        self.assertFalse(audit.in_range("0.14.0", "0.15.0+"))
        self.assertTrue(audit.in_range("0.20.0", "0.20.0"))
        self.assertFalse(audit.in_range("0.20.1", "0.20.0"))
        self.assertTrue(audit.in_range("0.11.13", "= 0.11.13"))

    def test_conjunctions(self):
        self.assertTrue(audit.in_range("0.24.9", ">= 0.1.0, < 0.29.3"))
        self.assertFalse(audit.in_range("0.29.3", ">= 0.1.0, < 0.29.3"))
        self.assertTrue(audit.in_range("0.27.0", ">= 0.26.0 && <= 0.29.0"))
        self.assertFalse(audit.in_range("0.25.0", ">= 0.26.0 && <= 0.29.0"))

    def test_upper_bound_written_first(self):
        # wasmtime の GHSA-ff4p-7xrq-q5r8 等の書き方（上限が先でも全部を満たす範囲）
        self.assertFalse(audit.in_range("36.0.20", "<= 6.0.0, >= 0.37.0"))
        self.assertTrue(audit.in_range("5.0.0", "<= 6.0.0, >= 0.37.0"))
        self.assertFalse(audit.in_range("0.36.0", "<= 6.0.0, >= 0.37.0"))
        self.assertFalse(audit.in_range("36.0.20", "<= 0.38.1, >=0.37.0"))
        self.assertTrue(audit.in_range("0.38.1", "<= 0.38.1, >=0.37.0"))

    def test_hyphen_range_includes_both_ends(self):
        self.assertTrue(audit.in_range("0.11.0", "0.11.0 - 0.11.6"))
        self.assertTrue(audit.in_range("0.11.3", "0.11.0 - 0.11.6"))
        self.assertTrue(audit.in_range("0.11.6", "0.11.0 - 0.11.6"))
        self.assertFalse(audit.in_range("0.11.7", "0.11.0 - 0.11.6"))

    def test_ranges_of_several_series(self):
        # quinn-udp の GHSA-6pp4-4cxf-xf88 の書き方（読んだままの「全部を満たす」では誰も該当しない）
        r = "< 0.5.16, >= 0.6.0 < 0.6.3"
        self.assertTrue(audit.in_range("0.5.14", r))
        self.assertFalse(audit.in_range("0.5.16", r))
        self.assertTrue(audit.in_range("0.6.2", r))
        self.assertFalse(audit.in_range("0.6.3", r))
        self.assertTrue(audit.in_range("0.11.13", "= 0.11.13, = 0.11.17"))
        self.assertTrue(audit.in_range("0.11.17", "= 0.11.13, = 0.11.17"))
        self.assertFalse(audit.in_range("0.11.14", "= 0.11.13, = 0.11.17"))

    def test_short_versions_compare_as_padded(self):
        self.assertTrue(audit.in_range("1.2.0", "= 1.2"))
        self.assertFalse(audit.in_range("1.2.0", "< 1.2"))

    def test_unreadable_range_raises(self):
        for r in ("", "unknown", "all versions", "> 0.6.0, < 0.5.0"):
            with self.assertRaises(ValueError):
                audit.in_range("0.5.14", r)


class PatchedInSeriesTests(unittest.TestCase):
    def test_same_series(self):
        self.assertTrue(audit.patched_in_series("0.5.16", ">= 0.5.16, >= 0.6.3"))
        self.assertFalse(audit.patched_in_series("0.5.14", ">= 0.5.16, >= 0.6.3"))
        self.assertTrue(audit.patched_in_series("36.0.20", "38.0.4, 37.0.3, 36.0.3"))
        self.assertFalse(audit.patched_in_series("36.0.2", "38.0.4, 37.0.3, 36.0.3"))


class RepoAdvisoriesTests(unittest.TestCase):
    def setUp(self):
        self._orig_get = audit._get

    def tearDown(self):
        audit._get = self._orig_get

    def _serve(self, advisories):
        audit._get = lambda url: advisories

    def test_several_series_range_is_reported(self):
        self._serve([{
            "ghsa_id": "GHSA-6pp4-4cxf-xf88",
            "summary": "Receive loop can spin after a truncated datagram",
            "vulnerabilities": [{
                "package": {"name": "quinn-udp"},
                "vulnerable_version_range": "< 0.5.16, >= 0.6.0 < 0.6.3",
                "patched_versions": ">= 0.5.16, >= 0.6.3",
            }],
        }])
        self.assertEqual([a["ghsa_id"] for a in audit.repo_advisories("quinn-rs/quinn", "quinn-udp", "0.5.14")],
                         ["GHSA-6pp4-4cxf-xf88"])
        self.assertEqual(audit.repo_advisories("quinn-rs/quinn", "quinn-udp", "0.5.16"), [])

    def test_other_packages_are_skipped(self):
        self._serve([{
            "ghsa_id": "GHSA-vr26-jcq5-fjj8",
            "summary": "quinn-proto only",
            "vulnerabilities": [{
                "package": {"name": "quinn-proto"},
                "vulnerable_version_range": "0.11.0 - 0.11.6",
                "patched_versions": "0.11.7",
            }],
        }])
        self.assertEqual(audit.repo_advisories("quinn-rs/quinn", "quinn-udp", "0.5.14"), [])

    def test_unreadable_range_is_reported_not_skipped(self):
        self._serve([{
            "ghsa_id": "GHSA-xxxx-xxxx-xxxx",
            "summary": "something",
            "vulnerabilities": [{
                "package": {"name": "quinn-udp"},
                "vulnerable_version_range": "see description",
                "patched_versions": "",
            }],
        }])
        found = audit.repo_advisories("quinn-rs/quinn", "quinn-udp", "0.5.16")
        self.assertEqual([a["ghsa_id"] for a in found], ["GHSA-xxxx-xxxx-xxxx"])
        self.assertIn("unreadable range", found[0]["summary"])

    def test_withdrawn_is_skipped(self):
        self._serve([{
            "ghsa_id": "GHSA-6pp4-4cxf-xf88",
            "withdrawn_at": "2026-10-01T00:00:00Z",
            "vulnerabilities": [{
                "package": {"name": "quinn-udp"},
                "vulnerable_version_range": "< 0.5.16",
                "patched_versions": ">= 0.5.16",
            }],
        }])
        self.assertEqual(audit.repo_advisories("quinn-rs/quinn", "quinn-udp", "0.5.14"), [])


if __name__ == "__main__":
    unittest.main()
