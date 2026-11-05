#!/usr/bin/env python3
"""F-290: Windows の実行ファイル（PE）にアイコンと版の情報のリソースがあるか確かめる。

    python3 tools/release/check-pe-resources.py <veil.exe> [...]

build.rs が packaging/windows/veil.rc から作った .res をリンクしていれば、リソースの表に RT_ICON・RT_GROUP_ICON・
RT_VERSION があり、版の情報に ProductName "veil" が入っている。標準のライブラリだけで動くので、x86_64・aarch64 の
どちらの PE も Linux の runner で確かめられる。llvm-cvtres が .res から作る COFF のオブジェクトも読める（手元の確認用）。
"""
import struct
import sys

RT_ICON, RT_GROUP_ICON, RT_VERSION = 3, 14, 16
NAMES = {RT_ICON: "RT_ICON", RT_GROUP_ICON: "RT_GROUP_ICON", RT_VERSION: "RT_VERSION"}


def resource_directory(path, data):
    """リソースの表の先頭（ファイル内の位置）を返す。"""
    if data[:2] == b"MZ":
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        if data[pe:pe + 4] != b"PE\0\0":
            sys.exit(f"{path}: not a PE file")
        coff = pe + 4
        nsec, = struct.unpack_from("<H", data, coff + 2)
        optsize, = struct.unpack_from("<H", data, coff + 16)
        opt = coff + 20
        magic, = struct.unpack_from("<H", data, opt)
        dd = opt + (112 if magic == 0x20B else 96)  # PE32+ / PE32 のデータディレクトリ
        rsrc_rva, rsrc_size = struct.unpack_from("<II", data, dd + 16)  # 3 番目＝リソースの表
        if not rsrc_rva or not rsrc_size:
            return None
        for i in range(nsec):
            s = opt + optsize + 40 * i
            vsize, va, rsize, raw = struct.unpack_from("<IIII", data, s + 8)
            if va <= rsrc_rva < va + max(vsize, rsize):
                return raw + rsrc_rva - va
        sys.exit(f"{path}: the resource table is outside every section")
    # COFF のオブジェクト（llvm-cvtres の出力）: .rsrc$01 の先頭がリソースの表
    nsec, = struct.unpack_from("<H", data, 2)
    optsize, = struct.unpack_from("<H", data, 16)
    for i in range(nsec):
        s = 20 + optsize + 40 * i
        if data[s:s + 8].rstrip(b"\0") == b".rsrc$01":
            return struct.unpack_from("<I", data, s + 20)[0]
    return None


def resource_types(data, base):
    named, ids = struct.unpack_from("<HH", data, base + 12)
    types = []
    for i in range(named + ids):
        name, _ = struct.unpack_from("<II", data, base + 16 + 8 * i)
        if not name & 0x80000000:
            types.append(name)
    return types


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    bad = 0
    for path in sys.argv[1:]:
        with open(path, "rb") as f:
            data = f.read()
        base = resource_directory(path, data)
        types = resource_types(data, base) if base is not None else []
        missing = [NAMES[t] for t in (RT_ICON, RT_GROUP_ICON, RT_VERSION) if t not in types]
        product = "ProductName\0".encode("utf-16-le")
        at = data.find(product)
        has_name = at >= 0 and "veil\0".encode("utf-16-le") in data[at:at + 64]
        if missing or not has_name:
            bad += 1
            why = f"no {', '.join(missing)}" if missing else "no ProductName \"veil\" in the version information"
            print(f"FAIL {path}: {why} (build.rs embeds packaging/windows/veil.rc when a resource compiler is found)")
        else:
            print(f"ok   {path}: icon and version information ({', '.join(NAMES[t] for t in sorted(NAMES))})")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
