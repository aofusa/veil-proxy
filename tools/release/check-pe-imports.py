#!/usr/bin/env python3
"""B-167: Windows の実行ファイル（PE）が、素の Windows に無い C ランタイムの DLL を読み込んでいないか確かめる。

    python3 tools/release/check-pe-imports.py <veil.exe> [...]

VCRUNTIME140.dll（Visual C++ 再頒布パッケージ）などを import していたら失敗する。veil は C ランタイムを静的に
リンクする（docker/Dockerfile.windows の +crt-static）。標準のライブラリ（PE の import 表の解析）だけで動くので、
x86_64・aarch64 のどちらの PE も Linux の runner で確かめられる。
"""
import re
import struct
import sys

# 素の Windows に無い（再頒布パッケージで入る）C/C++ ランタイム。api-ms-win-crt-* は UCRT の転送用の DLL で
# Windows 10 以降にはあるが、静的にリンクできていれば import されないので、混ざったことの印として一緒に見る
FORBIDDEN = re.compile(r"^(vcruntime\d+(_\d+)?|msvcp\d+(_\w+)?|concrt\d+|vccorlib\d+|ucrtbased?|api-ms-win-crt-.*)\.dll$", re.I)


def imports(path):
    data = open(path, "rb").read()
    if data[:2] != b"MZ":
        sys.exit(f"{path}: not a PE file")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        sys.exit(f"{path}: not a PE file")
    nsec, = struct.unpack_from("<H", data, pe + 6)
    optsize, = struct.unpack_from("<H", data, pe + 20)
    opt = pe + 24
    magic, = struct.unpack_from("<H", data, opt)
    dd = opt + (112 if magic == 0x20B else 96)  # PE32+ / PE32 のデータディレクトリ
    imp_rva, = struct.unpack_from("<I", data, dd + 8)  # 1 番目＝import 表
    secs = []
    for i in range(nsec):
        s = opt + optsize + 40 * i
        vsize, va, rsize, raw = struct.unpack_from("<IIII", data, s + 8)
        secs.append((va, max(vsize, rsize), raw))

    def off(rva):
        for va, size, raw in secs:
            if va <= rva < va + size:
                return raw + rva - va
        sys.exit(f"{path}: RVA {rva:#x} is outside every section")

    names = []
    if imp_rva:
        p = off(imp_rva)
        while True:
            ilt, _, _, name_rva, iat = struct.unpack_from("<IIIII", data, p)
            if not (ilt or name_rva or iat):
                break
            n = off(name_rva)
            names.append(data[n:data.index(b"\0", n)].decode("ascii"))
            p += 20
    return names


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    bad = 0
    for path in sys.argv[1:]:
        dlls = imports(path)
        hit = [d for d in dlls if FORBIDDEN.match(d)]
        if hit:
            bad += 1
            print(f"FAIL {path}: imports the C runtime DLLs {', '.join(hit)} "
                  "(link the C runtime statically: +crt-static)")
        else:
            print(f"ok   {path}: {len(dlls)} DLLs, no C runtime DLL ({', '.join(dlls)})")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
