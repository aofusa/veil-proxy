#!/usr/bin/env python3
"""F-253: FreeBSD ports・OpenBSD ports・NetBSD pkgsrc の veil の雛形を、版と Cargo.lock から作る。

    python3 tools/release/bsd-ports.py <tag> <out-dir>        # 例: v0.9.0

<out-dir>/{freebsd,openbsd,netbsd}/www/veil/ に、各 OS の ports の木へそのまま置ける port を作る。
- 依存のクレートの一覧は Cargo.lock から（FreeBSD: Makefile.crates、OpenBSD: crates.inc（ライセンス付き）、NetBSD: cargo-depends.mk）
- distinfo（チェックサム）は各 OS の `make makesum` で作る（release-bsd の試験・提出のとき）
雛形の元は packaging/bsd/ports/。各 OS の ports の木に取り込まれるまでは、ここで作ったものを使ってもらう。
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "packaging", "bsd", "ports")
# OpenBSD の _veil の uid/gid。ports の infrastructure/db/user.list に登録するとき（提出のとき）に空いている番号へ直す
OPENBSD_UID = "937"


def crates():
    lock = tomllib.load(open(os.path.join(ROOT, "Cargo.lock"), "rb"))
    out = []
    for p in lock["package"]:
        if p.get("source", "").startswith("registry+"):
            out.append((p["name"], p["version"]))
    return sorted(set(out))


def licenses():
    try:
        m = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--offline"],
                                      cwd=ROOT, capture_output=True, check=True, text=True).stdout)
    except Exception:
        return {}
    return {(p["name"], p["version"]): (p.get("license") or "") for p in m["packages"]}


def fill(src, dst, subs):
    s = open(src, encoding="utf-8").read()
    for k, v in subs.items():
        s = s.replace(f"@{k}@", v)
    if re.search(r"@[A-Z_]+@", s.replace("@RCD_SCRIPTS_SHELL@", "").replace("@PREFIX@", "").replace("@VARBASE@", "")
                 .replace("@PKG_SYSCONFDIR@", "")):
        sys.exit(f"unreplaced placeholder in {src}")
    open(dst, "w", encoding="utf-8").write(s)


def main():
    tag, out = sys.argv[1], sys.argv[2]
    ver = tag[1:] if tag.startswith("v") else tag
    # OpenBSD・NetBSD のパッケージの版: 0.9.0-rc.1 → 0.9.0rc1
    pkgver = re.sub(r"-rc\.?(\d+)$", r"rc\1", ver)
    cs = crates()
    lic = licenses()
    for osname in ("freebsd", "openbsd", "netbsd"):
        d = os.path.join(out, osname, "www", "veil")
        if os.path.exists(d):
            shutil.rmtree(d)
        shutil.copytree(os.path.join(SRC, osname, "www", "veil"), d)
        for dirpath, _, files in os.walk(d):
            for f in files:
                if f.endswith(".in") and not (osname == "freebsd" and dirpath.endswith("files")):
                    p = os.path.join(dirpath, f)
                    fill(p, p[:-3], {"VERSION": ver, "PKGVERSION": pkgver, "GH_TAG": tag, "GH_TAGNAME": "",
                                     "UID": OPENBSD_UID})
                    os.remove(p)
    # 依存のクレート
    with open(os.path.join(out, "freebsd/www/veil/Makefile.crates"), "w") as f:
        f.write("CARGO_CRATES=\t" + " \\\n\t\t".join(f"{n}-{v}" for n, v in cs) + "\n")
    with open(os.path.join(out, "openbsd/www/veil/crates.inc"), "w") as f:
        for n, v in cs:
            l = lic.get((n, v), "")
            f.write(f"MODCARGO_CRATES +=\t{n}\t{v}" + (f"\t# {l}" if l else "") + "\n")
    with open(os.path.join(out, "netbsd/www/veil/cargo-depends.mk"), "w") as f:
        f.write("# $NetBSD$\n\n")
        for n, v in cs:
            f.write(f"CARGO_CRATE_DEPENDS+=\t{n}-{v}\n")
    print(f"wrote ports for {tag} ({len(cs)} crates) to {out}")


if __name__ == "__main__":
    main()
