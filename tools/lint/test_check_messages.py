#!/usr/bin/env python3
"""Tests for check-messages.py（検査の側の誤りで素通りしないことを確かめる。F-242）。

使い方: python3 tools/lint/test_check_messages.py
"""

import importlib.util
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_messages", HERE / "check-messages.py")
cm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cm)


def found(src: str) -> list[str]:
    lits, _ = cm.scan(src)
    return [t for _, t in lits if cm.CJK.search(t) or cm.TICKET.search(t)]


class ScanTests(unittest.TestCase):
    def test_log_message_in_japanese_is_reported(self):
        self.assertEqual(found('fn f() { info!("起動した"); }'), ['"起動した"'])

    def test_ticket_number_is_reported(self):
        self.assertEqual(found('fn f() { warn!("ignored (B-52)"); }'), ['"ignored (B-52)"'])

    def test_ticket_like_words_are_not_reported(self):
        self.assertEqual(found('fn f() { info!("SHA-256 X-Forwarded-For HTTP-2 AF-12x"); }'), [])

    def test_comments_are_ignored(self):
        src = 'fn f() {\n    // 日本語のコメント（F-153）\n    /* ブロック B-1 */\n    info!("ok");\n}\n'
        self.assertEqual(found(src), [])

    def test_cfg_test_module_is_ignored(self):
        src = 'fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert!(true, "日本語"); }\n}\n'
        self.assertEqual(found(src), [])

    def test_test_fn_outside_module_is_ignored(self):
        self.assertEqual(found('#[test]\nfn t() { panic!("失敗"); }\nfn g() { info!("ok"); }'), [])

    def test_code_after_test_module_is_still_checked(self):
        src = '#[cfg(test)]\nmod tests { fn t() { let _ = "日本語"; } }\nfn g() { error!("失敗"); }\n'
        self.assertEqual(found(src), ['"失敗"'])

    def test_raw_string_with_quote_does_not_shift_tokens(self):
        src = 'fn f() { let a = r#"say "hi""#; info!("起動"); }'
        self.assertEqual(found(src), ['"起動"'])

    def test_char_literal_quote_does_not_start_string(self):
        src = "fn f() { let q = '\"'; let a = \"ok\"; let b = \"日本\"; }"
        self.assertEqual(found(src), ['"日本"'])

    def test_lifetime_does_not_break_lexing(self):
        src = "fn f<'a>(x: &'a str) -> &'a str { info!(\"起動\"); x }"
        self.assertEqual(found(src), ['"起動"'])

    def test_doc_attribute_string_is_checked(self):
        self.assertEqual(found('#[doc = "説明"]\npub fn f() {}'), ['"説明"'])

    def test_escaped_quote_in_string(self):
        self.assertEqual(found('fn f() { info!("a \\" b"); warn!("日本"); }'), ['"日本"'])

    def test_test_only_file_is_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text('#[cfg(test)]\nmod legacy_tests;\npub fn f() { info!("ok"); }\n')
            (root / "src" / "legacy_tests.rs").write_text('fn t() { assert!(false, "日本語"); }\n')
            (root / "src" / "a.rs").write_text('pub fn g() { info!("日本語"); }\n')
            problems = cm.check(root)
            self.assertEqual(len(problems), 1)
            self.assertTrue(problems[0].startswith("src/a.rs:1:"))


if __name__ == "__main__":
    unittest.main()
