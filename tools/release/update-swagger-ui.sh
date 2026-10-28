#!/usr/bin/env bash
# F-264: ライブラリの `/docs`（Swagger UI）に同梱する swagger-ui-dist を更新する。
#
#   tools/release/update-swagger-ui.sh <version>     # 例: 5.34.0（npm のレジストリから取得して置き換える）
#   tools/release/update-swagger-ui.sh --check       # 同梱物と README.veil.md・assets.rs の sha256 が合っているか
#   tools/release/update-swagger-ui.sh --audit       # 同梱の版に npm の advisory が無いか（直す版の無いものも失敗）
#
# 置き場所: third_party/swagger-ui-dist/<version>/ に gzip・brotli で事前圧縮した 2 ファイル（生の JS・CSS は置かない）と
# LICENSE・NOTICE、third_party/swagger-ui-dist/README.veil.md（取得元・sha256）と assets.rs（Rust が include! する定数）。
# 取得した tarball は npm の `integrity`（sha512）で検査する。バンドルの末尾の `//# sourceMappingURL=` は取り除く
# （ブラウザの開発者ツールが外へ取りに行かないように）。
#
# 前提: curl・tar・node（gzip・brotli・sha256・sha512 は node の zlib・crypto で計算する）。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="$ROOT/third_party/swagger-ui-dist"
FILES="swagger-ui-bundle.js swagger-ui.css"

current_version() { sed -n 's/^pub const VERSION: &str = "\(.*\)";$/\1/p' "$DIR/assets.rs"; }

case "${1:-}" in
  ""|-h|--help) sed -n 2,14p "$0"; exit 0 ;;
  --check)
    V="$(current_version)"
    node - "$DIR" "$V" $FILES <<'JS'
const [dir, v, ...files] = process.argv.slice(2);
const fs = require('fs'), zlib = require('zlib'), crypto = require('crypto');
const rs = fs.readFileSync(`${dir}/assets.rs`, 'utf8');
const readme = fs.readFileSync(`${dir}/README.veil.md`, 'utf8');
let bad = 0;
for (const f of files) {
  const gz = zlib.gunzipSync(fs.readFileSync(`${dir}/${v}/${f}.gz`));
  const br = zlib.brotliDecompressSync(fs.readFileSync(`${dir}/${v}/${f}.br`));
  const h = crypto.createHash('sha256').update(gz).digest('hex');
  if (!gz.equals(br)) { console.error(`FAIL ${f}: .gz and .br differ`); bad++; }
  if (!rs.includes(h)) { console.error(`FAIL ${f}: sha256 ${h} is not in assets.rs`); bad++; }
  if (!readme.includes(h)) { console.error(`FAIL ${f}: sha256 ${h} is not in README.veil.md`); bad++; }
  if (/sourceMappingURL/.test(gz.toString('latin1'))) { console.error(`FAIL ${f}: still has a sourceMappingURL`); bad++; }
  if (!bad) console.log(`ok   ${f} ${h}`);
}
process.exit(bad ? 1 : 0);
JS
    exit $? ;;
  --audit)
    V="$(current_version)"
    # npm の bulk advisory API（`npm audit` が使うもの）。該当があれば直す版の有無にかかわらず失敗にする
    OUT="$(curl -fsS -X POST -H 'content-type: application/json' \
      -d "{\"swagger-ui-dist\":[\"$V\"]}" https://registry.npmjs.org/-/npm/v1/security/advisories/bulk)"
    if [ "$OUT" = "{}" ]; then echo "ok   swagger-ui-dist $V: no advisories"; exit 0; fi
    echo "FAIL swagger-ui-dist $V has advisories: $OUT"; exit 1 ;;
esac

V="$1"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
curl -fsS "https://registry.npmjs.org/swagger-ui-dist/$V" > "$TMP/meta.json"
URL="$(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1])).dist.tarball)' "$TMP/meta.json")"
INTEGRITY="$(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1])).dist.integrity)' "$TMP/meta.json")"
curl -fsS "$URL" -o "$TMP/pkg.tgz"
GOT="sha512-$(node -e 'console.log(require("crypto").createHash("sha512").update(require("fs").readFileSync(process.argv[1])).digest("base64"))' "$TMP/pkg.tgz")"
[ "$GOT" = "$INTEGRITY" ] || { echo "integrity mismatch: $GOT != $INTEGRITY" >&2; exit 1; }
tar -xzf "$TMP/pkg.tgz" -C "$TMP"

rm -rf "$DIR"/[0-9]*
mkdir -p "$DIR/$V"
cp "$TMP/package/LICENSE" "$DIR/$V/LICENSE"
[ -f "$TMP/package/NOTICE" ] && cp "$TMP/package/NOTICE" "$DIR/$V/NOTICE"
node - "$TMP/package" "$DIR" "$V" "$URL" "$INTEGRITY" $FILES <<'JS'
const [src, dir, v, url, integrity, ...files] = process.argv.slice(2);
const fs = require('fs'), zlib = require('zlib'), crypto = require('crypto');
const rows = [], consts = [];
for (const f of files) {
  let raw = fs.readFileSync(`${src}/${f}`, 'latin1');
  // source map の参照（`//# sourceMappingURL=...`・`/*# sourceMappingURL=... */`）を取り除く
  raw = raw.replace(/\n?\/\/# sourceMappingURL=[^\n]*\s*$/, '\n').replace(/\n?\/\*# sourceMappingURL=[^*]*\*\/\s*$/, '\n');
  const buf = Buffer.from(raw, 'latin1');
  const gz = zlib.gzipSync(buf, { level: 9 });
  const br = zlib.brotliCompressSync(buf, { params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 11, [zlib.constants.BROTLI_PARAM_SIZE_HINT]: buf.length } });
  fs.writeFileSync(`${dir}/${v}/${f}.gz`, gz);
  fs.writeFileSync(`${dir}/${v}/${f}.br`, br);
  const h = crypto.createHash('sha256').update(buf).digest('hex');
  rows.push(`| \`${f}\` | ${buf.length} | ${gz.length} | ${br.length} | \`${h}\` |`);
  const id = f.replace(/^swagger-ui-?/, '').replace(/[.-]/g, '_').replace(/^_+/, '').toUpperCase();
  consts.push(`/// sha256 of \`${f}\` (uncompressed).\npub const ${id}_SHA256: &str = "${h}";`);
  consts.push(`/// \`${f}\`, gzip.\npub static ${id}_GZ: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/third_party/swagger-ui-dist/${v}/${f}.gz"));`);
  consts.push(`/// \`${f}\`, brotli.\npub static ${id}_BR: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/third_party/swagger-ui-dist/${v}/${f}.br"));`);
}
fs.writeFileSync(`${dir}/assets.rs`,
`// Generated by tools/release/update-swagger-ui.sh. Do not edit.
/// The bundled swagger-ui-dist version.
pub const VERSION: &str = "${v}";
${consts.join('\n')}
`);
fs.writeFileSync(`${dir}/README.veil.md`,
`# swagger-ui-dist（veil が同梱する Swagger UI、F-264）

ライブラリ（\`openapi-ui-embedded\` feature）の \`/docs\` が配る Swagger UI。本体（\`full\` 等）のバイナリには入らない。
更新は \`tools/release/update-swagger-ui.sh <版>\`、検査は \`--check\`（sha256）・\`--audit\`（npm の advisory）。

- 版: ${v}
- 取得元: ${url}
- npm の integrity: \`${integrity}\`
- ライセンス: Apache-2.0（\`${v}/LICENSE\`${fs.existsSync(`${dir}/${v}/NOTICE`) ? `・\`${v}/NOTICE\`` : ''}）
- 変更: 末尾の source map の参照（\`sourceMappingURL\`）を取り除いた。それ以外は取得したままのものを gzip（level 9）と
  brotli（quality 11）で圧縮して置く（生のファイルは置かない）。\`swagger-ui-standalone-preset.js\` は使わない。

| ファイル | バイト数 | gzip | brotli | sha256（取り除いた後・圧縮前） |
|---|---:|---:|---:|---|
${rows.join('\n')}
`);
JS
"$0" --check
