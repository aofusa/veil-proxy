# swagger-ui-dist（veil が同梱する Swagger UI、F-264）

ライブラリ（`openapi-ui-embedded` feature）の `/docs` が配る Swagger UI。本体（`full` 等）のバイナリには入らない。
更新は `tools/release/update-swagger-ui.sh <版>`、検査は `--check`（sha256）・`--audit`（npm の advisory）。

- 版: 5.34.0
- 取得元: https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-5.34.0.tgz
- npm の integrity: `sha512-H6tAIjr3XqE31PbkZl0OFya1ziRLDfTwh3J6OHdumPtPCLfjTnyLeagl/55kFNZcTOz9a9QI9WP0FXldwiGJag==`
- ライセンス: Apache-2.0（`5.34.0/LICENSE`・`5.34.0/NOTICE`）
- 変更: 末尾の source map の参照（`sourceMappingURL`）を取り除いた。それ以外は取得したままのものを gzip（level 9）と
  brotli（quality 11）で圧縮して置く（生のファイルは置かない）。`swagger-ui-standalone-preset.js` は使わない。

| ファイル | バイト数 | gzip | brotli | sha256（取り除いた後・圧縮前） |
|---|---:|---:|---:|---|
| `swagger-ui-bundle.js` | 1684244 | 449176 | 353627 | `3f6abf70480db7e07914df4c98a77385f22ea585eaf457a3e6415ba0eae2d167` |
| `swagger-ui.css` | 187535 | 26981 | 19823 | `66c411689f69d3c3bbe327a66f1ef09a0472a80896b2be8452ff86fec38a7fee` |
