#!/usr/bin/env python3
"""Check that user-visible strings in the Rust sources are English-only and carry no ticket numbers.

veil の実行時に出る文字列（ログ・エラー・panic・`expect`・`#[doc = ..]`）は英語で書き、チケット番号
（`F-153`・`B-52` など）を入れない（F-242）。テストの中（`#[cfg(test)]`・`#[test]` の項目、`#[cfg(test)] mod x;` で
読まれるファイル）は対象外。コメント（`//`・`/* */`）も対象外（日本語でよい）。

使い方: python3 tools/lint/check-messages.py [ROOT]   （ROOT の既定はリポジトリのルート）
見つかれば `ファイル:行: 文字列` を出して終了コード 1。
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

CJK = re.compile(r"[぀-ヿ㐀-䶿一-鿿＀-￯]")
TICKET = re.compile(r"(?<![A-Za-z0-9_])[FB]-[0-9]+(?![0-9])")
# テストの項目を示す属性（`#[cfg(test)]`・`#[cfg(all(test, ..))]`・`#[test]`・`#[tokio::test]` など）。
TEST_ATTR = re.compile(r"^#\s*\[\s*(cfg\s*\(\s*(all\s*\(\s*)?test\b|([a-z_]+::)?test\s*\])")


class Lexer:
    """Rust の字句を、文字列・文字・コメントを区別できる程度に読む。"""

    def __init__(self, src: str):
        self.s = src
        self.i = 0
        self.n = len(src)

    def line_of(self, pos: int) -> int:
        return self.s.count("\n", 0, pos) + 1

    def tokens(self):
        """(kind, start, text) を順に返す。kind は str / attr / open / close / semi / other。"""
        s, n = self.s, self.n
        while self.i < n:
            c = s[self.i]
            if c.isspace():
                self.i += 1
                continue
            if s.startswith("//", self.i):
                j = s.find("\n", self.i)
                self.i = n if j < 0 else j
                continue
            if s.startswith("/*", self.i):
                depth, j = 1, self.i + 2
                while j < n and depth:
                    if s.startswith("/*", j):
                        depth, j = depth + 1, j + 2
                    elif s.startswith("*/", j):
                        depth, j = depth - 1, j + 2
                    else:
                        j += 1
                self.i = j
                continue
            m = re.compile(r'(b|c)?r(#*)"').match(s, self.i)
            if m and (self.i == 0 or not (s[self.i - 1].isalnum() or s[self.i - 1] == "_")):
                end = '"' + m.group(2)
                j = s.find(end, m.end())
                j = n if j < 0 else j + len(end)
                yield ("str", self.i, s[self.i:j])
                self.i = j
                continue
            if c == '"' or (c in "bc" and s.startswith('"', self.i + 1)
                            and (self.i == 0 or not (s[self.i - 1].isalnum() or s[self.i - 1] == "_"))):
                start = self.i
                j = self.i + (2 if c != '"' else 1)
                while j < n and s[j] != '"':
                    j += 2 if s[j] == "\\" else 1
                yield ("str", start, s[start:j + 1])
                self.i = j + 1
                continue
            if c == "'":
                # 文字リテラルかライフタイムか。
                m = re.compile(r"'(\\(x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'])'").match(s, self.i)
                if m:
                    self.i = m.end()
                    continue
                self.i += 1
                continue
            if c == "#" and (s.startswith("[", self.i + 1) or s.startswith("![", self.i + 1)):
                start = self.i
                j = s.index("[", self.i) + 1
                depth = 1
                inner = []
                while j < n and depth:
                    if s[j] == "[":
                        depth += 1
                    elif s[j] == "]":
                        depth -= 1
                    elif s[j] == '"':
                        # 属性の中の文字列も検査する（`#[doc = ".."]`・`#[error("..")]` など）
                        k = j + 1
                        while k < n and s[k] != '"':
                            k += 2 if s[k] == "\\" else 1
                        inner.append((j, s[j:k + 1]))
                        j = k
                    j += 1
                yield ("attr", start, (s[start:j], inner))
                self.i = j
                continue
            if c == "{":
                yield ("open", self.i, c)
            elif c == "}":
                yield ("close", self.i, c)
            elif c == ";":
                yield ("semi", self.i, c)
            else:
                yield ("other", self.i, c)
            self.i += 1


def scan(src: str):
    """テスト以外の文字列のリテラルを (位置, 文字列) で返す。"""
    lx = Lexer(src)
    out = []
    toks = list(lx.tokens())
    i = 0
    pending_test = False
    while i < len(toks):
        kind, pos, val = toks[i]
        if kind == "attr":
            text, inner = val
            if TEST_ATTR.match(text):
                pending_test = True
            elif not pending_test:
                out.extend(inner)
            i += 1
            continue
        if pending_test:
            # 次の項目を `;` か対応する `}` まで読み飛ばす。
            depth = 0
            while i < len(toks):
                k = toks[i][0]
                if k == "open":
                    depth += 1
                elif k == "close":
                    depth -= 1
                    if depth == 0:
                        break
                elif k == "semi" and depth == 0:
                    break
                i += 1
            pending_test = False
            i += 1
            continue
        if kind == "str":
            out.append((pos, val))
        i += 1
    return out, lx


TEST_MOD = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z_0-9]+)\s*;")


def test_only_paths(files: list[Path]) -> set[Path]:
    """`#[cfg(test)] mod x;` で読まれるファイル（と、そのディレクトリ）を集める。"""
    out: set[Path] = set()
    for f in files:
        src = f.read_text(encoding="utf-8")
        for m in TEST_MOD.finditer(src):
            name = m.group(1)
            if f.name in ("mod.rs", "lib.rs", "main.rs"):
                base = f.parent
            else:
                base = f.parent / f.stem
            out.add(base / f"{name}.rs")
            out.add(base / name)
    return out


def is_under(p: Path, roots: set[Path]) -> bool:
    return any(p == r or r in p.parents for r in roots)


def collect(root: Path) -> list[Path]:
    files: list[Path] = []
    for top in [root / "src"] + sorted((root / "crates").glob("*/src")):
        for dirpath, dirnames, filenames in os.walk(top):
            dirnames[:] = [d for d in dirnames if d not in ("tests", "testdata", "target")]
            for fn in filenames:
                if fn.endswith(".rs"):
                    files.append(Path(dirpath) / fn)
    for b in [root / "build.rs"] + sorted((root / "crates").glob("*/build.rs")):
        if b.exists():
            files.append(b)
    return files


def check(root: Path) -> list[str]:
    files = collect(root)
    skip = test_only_paths(files)
    problems = []
    for f in sorted(files):
        if f.name in ("tests.rs",) or f.name.endswith("_tests.rs") or is_under(f, skip):
            continue
        src = f.read_text(encoding="utf-8")
        lits, lx = scan(src)
        for pos, text in lits:
            if CJK.search(text) or TICKET.search(text):
                shown = " ".join(text.split())
                if len(shown) > 160:
                    shown = shown[:157] + "..."
                problems.append(f"{f.relative_to(root)}:{lx.line_of(pos)}: {shown}")
    return problems


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[2]
    problems = check(root)
    for p in problems:
        print(p)
    if problems:
        print(f"\n{len(problems)} string(s) contain non-English text or ticket numbers "
              "(write messages in English without ticket IDs; move the ID to a // comment)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
