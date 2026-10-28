#!/usr/bin/env bash
# F-241: 利用者が読む API の文書が英語だけで、チケット番号（F-123・B-45 など）を含まないことを確かめる。
#
#   tools/release/check-docs.sh            # Rust（docs.rs と同じ構成の rustdoc の HTML）+ Python のソース
#   tools/release/check-docs.sh --python   # Python のソースだけ（veil-api と veil-api-kit の README・docstring・PyO3 の doc・.pyi）
#   tools/release/check-docs.sh --wheel    # 入れた veil-api の help() に出る __doc__（wheel の試験の後に。PYTHON で python を選ぶ）
#
# Rust は `cargo +nightly doc`（docs.rs と同じ --cfg docsrs）で作った target/doc の HTML の本文と検索の索引を見る。
# nightly が無ければ stable で作る（feature の表示が出ないだけで本文は同じ）。docs.rs はソースの表示を
# 出さない（`doc(html_no_source)`）ので target/doc/src/ は見ない。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

mode="${1:-all}"
fail=0

check_python_sources() {
    python3 - "$ROOT" <<'PY' || return 1
import ast, re, sys
from pathlib import Path
root = Path(sys.argv[1])
CJK = re.compile(r"[぀-ヿ㐀-䶿一-鿿＀-￯]")
TICKET = re.compile(r"(?<![A-Za-z0-9_])[FB]-[0-9]+(?![0-9])")
bad = []
def chk(where, text):
    if CJK.search(text) or TICKET.search(text):
        bad.append(f"{where}: {' '.join(text.split())[:150]}")
py = root / "crates/veil-python"
for n, line in enumerate((py / "README.md").read_text(encoding="utf-8").splitlines(), 1):
    chk(f"crates/veil-python/README.md:{n}", line)
kit = root / "crates/veil-python-kit"
for n, line in enumerate((kit / "README.md").read_text(encoding="utf-8").splitlines(), 1):
    chk(f"crates/veil-python-kit/README.md:{n}", line)
for f in sorted((py / "python/veil").glob("*.py*")) + sorted((kit / "src/veil_kit").glob("*.py")):
    tree = ast.parse(f.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            d = ast.get_docstring(node)
            if d:
                chk(f"{f.relative_to(root)}:{getattr(node, 'lineno', 1)}", d)
for f in sorted((py / "src").glob("*.rs")):
    for n, line in enumerate(f.read_text(encoding="utf-8").splitlines(), 1):
        s = line.strip()
        if s.startswith("///") or s.startswith("//!"):
            chk(f"{f.relative_to(root)}:{n}", s)
for b in bad:
    print(b)
if bad:
    print(f"\n{len(bad)} Python doc line(s) contain non-English text or ticket numbers", file=sys.stderr)
    sys.exit(1)
PY
}

check_html() {
    python3 - "$ROOT/target/doc" <<'PY' || return 1
import html, re, sys
from pathlib import Path
doc = Path(sys.argv[1])
CJK = re.compile(r"[぀-ヿ㐀-䶿一-鿿＀-￯]")
TICKET = re.compile(r"(?<![A-Za-z0-9_])[FB]-[0-9]+(?![0-9])")
TAG = re.compile(r"<script\b.*?</script>|<style\b.*?</style>|<[^>]+>", re.S)
bad = []
crates = ["veil", "veil_macros", "veil_build", "veil_kit"]
files = []
for c in crates:
    if not (doc / c).is_dir():
        print(f"missing {doc / c} (rustdoc did not run?)", file=sys.stderr)
        sys.exit(2)
    files += sorted((doc / c).rglob("*.html"))
    files += sorted((doc / "search.desc" / c).glob("*.js")) if (doc / "search.desc" / c).is_dir() else []
for f in files:
    raw = f.read_text(encoding="utf-8", errors="replace")
    text = html.unescape(TAG.sub(" ", raw)) if f.suffix == ".html" else raw
    for m in list(CJK.finditer(text))[:1] + list(TICKET.finditer(text))[:1]:
        a = max(0, m.start() - 60)
        bad.append(f"{f.relative_to(doc)}: ...{' '.join(text[a:m.end() + 60].split())}...")
for b in bad:
    print(b)
print(f"checked {len(files)} files", file=sys.stderr)
if bad:
    print(f"\n{len(bad)} rendered doc page(s) contain non-English text or ticket numbers", file=sys.stderr)
    sys.exit(1)
PY
}

check_wheel() {
    "${PYTHON:-python3}" - <<'PY' || return 1
import inspect, re, sys
import veil
CJK = re.compile(r"[぀-ヿ㐀-䶿一-鿿＀-￯]")
TICKET = re.compile(r"(?<![A-Za-z0-9_])[FB]-[0-9]+(?![0-9])")
mods = [veil]
for name in ("_veil", "grpc"):
    try:
        mods.append(__import__(f"veil.{name}", fromlist=["x"]))
    except Exception:
        pass
seen, bad = set(), []
def visit(obj, path, depth=0):
    if id(obj) in seen or depth > 3:
        return
    seen.add(id(obj))
    d = getattr(obj, "__doc__", None)
    if isinstance(d, str) and (CJK.search(d) or TICKET.search(d)):
        bad.append(f"{path}: {' '.join(d.split())[:150]}")
    if inspect.ismodule(obj) or inspect.isclass(obj):
        for k, v in vars(obj).items():
            if k.startswith("_") and k not in ("__init__", "__call__"):
                continue
            if inspect.ismodule(v) and not v.__name__.startswith("veil"):
                continue
            if inspect.isclass(v) and not (v.__module__ or "").startswith("veil"):
                continue
            visit(v, f"{path}.{k}", depth + 1)
for m in mods:
    visit(m, m.__name__)
for b in bad:
    print(b)
print(f"checked {len(seen)} objects", file=sys.stderr)
if bad:
    sys.exit(1)
PY
}

if [ "$mode" = "--wheel" ]; then
    check_wheel
    exit $?
fi

check_python_sources || fail=1
if [ "$mode" != "--python" ]; then
    rm -rf target/doc/veil target/doc/veil_macros target/doc/veil_build target/doc/veil_kit
    if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --no-default-features --features full,api,veil-proxy-kit/full \
            -p veil-proxy -p veil-proxy-macros -p veil-proxy-build -p veil-proxy-kit
    else
        cargo doc --no-deps --no-default-features --features full,api,veil-proxy-kit/full \
            -p veil-proxy -p veil-proxy-macros -p veil-proxy-build -p veil-proxy-kit
    fi
    check_html || fail=1
fi
exit $fail
