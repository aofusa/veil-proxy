#!/usr/bin/env python3
"""Generate HPACK Huffman 4-bit LUT decode table from HUFFMAN_ENCODE_TABLE.

Source of truth: src/http2/hpack/huffman.rs (HUFFMAN_ENCODE_TABLE).
Output: src/http2/hpack/huffman_decode_table.rs (@generated, packed u32).

Usage:
  python3 tools/gen_huffman_decode_table.py \\
    --stride 4 \\
    --parse-from src/http2/hpack/huffman.rs \\
    --out src/http2/hpack/huffman_decode_table.rs

  python3 tools/gen_huffman_decode_table.py --stride 4 \\
    --parse-from src/http2/hpack/huffman.rs --check
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# flags: NEED=0x00, ACCEPT=0x01, ERROR=0x02
FLAG_NEED = 0x00
FLAG_ACCEPT = 0x01
FLAG_ERROR = 0x02

EOS_SYM = 256


@dataclass
class Node:
    """Binary trie node. leaf_sym is set for leaves; children for internals."""

    left: Optional["Node"] = None
    right: Optional["Node"] = None
    leaf_sym: Optional[int] = None
    state_id: Optional[int] = None  # only for internal nodes

    def is_leaf(self) -> bool:
        return self.leaf_sym is not None


def parse_encode_table(source: str) -> list[tuple[int, int]]:
    """Parse HUFFMAN_ENCODE_TABLE entries: (0x..., N) lines."""
    # Match the table block first for robustness.
    table_match = re.search(
        r"static\s+HUFFMAN_ENCODE_TABLE\s*:\s*\[\(u32,\s*u8\);\s*257\]\s*=\s*\[(.*?)\];",
        source,
        re.DOTALL,
    )
    if not table_match:
        raise SystemExit("failed to locate HUFFMAN_ENCODE_TABLE in source")

    body = table_match.group(1)
    # (0x1ff8, 13) or (0x0, 5) etc.
    entries = re.findall(
        r"\(\s*(0x[0-9a-fA-F]+|\d+)\s*,\s*(\d+)\s*\)",
        body,
    )
    if len(entries) != 257:
        raise SystemExit(f"expected 257 encode entries, got {len(entries)}")

    out: list[tuple[int, int]] = []
    for code_s, len_s in entries:
        code = int(code_s, 0)
        length = int(len_s)
        if length < 1 or length > 30:
            raise SystemExit(f"invalid code length {length}")
        out.append((code, length))
    return out


def build_trie(table: list[tuple[int, int]]) -> Node:
    root = Node()
    for sym, (code, length) in enumerate(table):
        node = root
        for i in range(length - 1, -1, -1):
            bit = (code >> i) & 1
            if node.is_leaf():
                raise SystemExit(f"prefix conflict inserting sym={sym}")
            if bit == 0:
                if node.left is None:
                    node.left = Node()
                node = node.left
            else:
                if node.right is None:
                    node.right = Node()
                node = node.right
        if node.left is not None or node.right is not None:
            raise SystemExit(f"prefix conflict at leaf sym={sym}")
        if node.leaf_sym is not None:
            raise SystemExit(f"duplicate leaf for sym={sym}")
        node.leaf_sym = sym
    return root


def assign_state_ids(root: Node) -> list[Node]:
    """BFS assign state ids 0..N-1 to internal nodes; root = 0."""
    internals: list[Node] = []
    queue = [root]
    while queue:
        n = queue.pop(0)
        if n.is_leaf():
            continue
        n.state_id = len(internals)
        internals.append(n)
        if n.left is not None:
            queue.append(n.left)
        if n.right is not None:
            queue.append(n.right)
    if len(internals) != 256:
        raise SystemExit(f"expected 256 internal nodes, got {len(internals)}")
    if root.state_id != 0:
        raise SystemExit("root state_id must be 0")
    return internals


def pack_entry(flags: int, sym: int, bits: int, next_state: int) -> int:
    """flags | sym<<8 | bits<<16 | next<<24 (little-endian field layout)."""
    assert 0 <= flags <= 0xFF
    assert 0 <= sym <= 0xFF
    assert 0 <= bits <= 0xFF
    assert 0 <= next_state <= 0xFF
    return flags | (sym << 8) | (bits << 16) | (next_state << 24)


def walk_peek(node: Node, peek: int, stride: int) -> int:
    """Walk up to `stride` bits MSB-first from `node` for peek value.

    Returns packed u32 DecodeEntry.
    """
    cur = node
    for consumed in range(1, stride + 1):
        bit = (peek >> (stride - consumed)) & 1
        child = cur.left if bit == 0 else cur.right
        if child is None:
            # missing edge → ERROR
            return pack_entry(FLAG_ERROR, 0, 0, 0)
        if child.is_leaf():
            sym = child.leaf_sym
            assert sym is not None
            if sym == EOS_SYM:
                # never emit EOS
                return pack_entry(FLAG_ERROR, 0, 0, 0)
            # ACCEPT: next = root
            return pack_entry(FLAG_ACCEPT, sym, consumed, 0)
        cur = child
    # still internal after stride bits → NEED
    assert cur.state_id is not None
    return pack_entry(FLAG_NEED, 0, stride, cur.state_id)


def generate_table(internals: list[Node], stride: int) -> list[list[int]]:
    peek_count = 1 << stride
    table: list[list[int]] = []
    for state_node in internals:
        row: list[int] = []
        for peek in range(peek_count):
            entry = walk_peek(state_node, peek, stride)
            # invariants
            flags = entry & 0xFF
            bits = (entry >> 16) & 0xFF
            next_s = (entry >> 24) & 0xFF
            assert flags in (FLAG_NEED, FLAG_ACCEPT, FLAG_ERROR)
            if flags == FLAG_ERROR:
                assert bits == 0
            else:
                assert 1 <= bits <= stride
            if flags == FLAG_ACCEPT:
                assert next_s == 0  # root
            if flags == FLAG_NEED:
                assert bits == stride
            assert next_s < len(internals)
            row.append(entry)
        table.append(row)
    return table


def emit_rust(table: list[list[int]], stride: int) -> str:
    state_count = len(table)
    peek_count = 1 << stride
    lines: list[str] = []
    lines.append(
        "// @generated by tools/gen_huffman_decode_table.py — DO NOT EDIT"
    )
    lines.append("//")
    lines.append(
        "// HPACK Huffman decode LUT (packed u32 form A)."
    )
    lines.append(
        f"// STRIDE={stride}, states={state_count}, peeks={peek_count}, "
        f"entries={state_count * peek_count}"
    )
    lines.append(
        "// packing: flags | sym<<8 | bits<<16 | next<<24"
    )
    lines.append(
        "// flags: NEED=0x00, ACCEPT=0x01, ERROR=0x02"
    )
    lines.append("")
    lines.append(
        f"pub(super) const HUFFMAN_DECODE_STRIDE: u32 = {stride};"
    )
    lines.append(
        f"pub(super) const HUFFMAN_DECODE_PEEK_MASK: u32 = "
        f"{(1 << stride) - 1};"
    )
    lines.append(
        f"pub(super) const HUFFMAN_DECODE_PEEK_COUNT: usize = {peek_count};"
    )
    lines.append(
        f"pub(super) const HUFFMAN_DECODE_STATE_COUNT: usize = {state_count};"
    )
    lines.append("")
    lines.append(
        "/// パック済みデコード LUT: `[[u32; PEEK_COUNT]; STATE_COUNT]`。"
    )
    lines.append(
        "/// 各 `u32` は `flags | sym<<8 | bits<<16 | next<<24`。"
    )
    lines.append("#[rustfmt::skip]")
    lines.append(
        "pub(super) static HUFFMAN_DECODE_TABLE_PACKED: "
        f"[[u32; {peek_count}]; {state_count}] = ["
    )

    # Compact hex output: 8 entries per line within each state row.
    per_line = 8
    for s, row in enumerate(table):
        lines.append(f"    // state {s}")
        lines.append("    [")
        for i in range(0, peek_count, per_line):
            chunk = row[i : i + per_line]
            hexes = ", ".join(f"0x{v:08x}" for v in chunk)
            if i + per_line < peek_count:
                lines.append(f"        {hexes},")
            else:
                lines.append(f"        {hexes},")
        lines.append("    ],")
    lines.append("];")
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--stride", type=int, default=4, choices=[4, 8])
    ap.add_argument(
        "--parse-from",
        type=Path,
        default=Path("src/http2/hpack/huffman.rs"),
        help="Path to huffman.rs containing HUFFMAN_ENCODE_TABLE",
    )
    ap.add_argument(
        "--out",
        type=Path,
        default=Path("src/http2/hpack/huffman_decode_table.rs"),
        help="Output Rust source path",
    )
    ap.add_argument(
        "--check",
        action="store_true",
        help="Verify existing --out is byte-identical to regeneration",
    )
    args = ap.parse_args()

    source = args.parse_from.read_text(encoding="utf-8")
    table = parse_encode_table(source)
    root = build_trie(table)
    internals = assign_state_ids(root)
    packed = generate_table(internals, args.stride)
    rust = emit_rust(packed, args.stride)

    if args.check:
        if not args.out.exists():
            print(f"missing generated file: {args.out}", file=sys.stderr)
            return 1
        existing = args.out.read_text(encoding="utf-8")
        if existing != rust:
            print(
                f"generated content differs from {args.out}",
                file=sys.stderr,
            )
            return 1
        print(f"OK: {args.out} is up to date")
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(rust, encoding="utf-8")
    print(
        f"wrote {args.out} "
        f"({len(packed)} states × {1 << args.stride} peeks, "
        f"{len(rust)} bytes source)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
