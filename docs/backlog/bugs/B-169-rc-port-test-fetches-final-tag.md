# B-169: rc のタグでは BSD の port の試験が本番のタグのソースを取りに行き、404 で失敗する

## 出典

v0.9.2-rc.1 の release の run（40189832094、2026-10-25）。FreeBSD・OpenBSD・NetBSD の port の試験がすべて
`make makesum` で失敗した（`https://codeload.github.com/aofusa/veil-proxy/tar.gz/refs/tags/v0.9.2: 404 Not Found`）。

## 事象

- dist.yml の `bsd-ports` は `inputs.ref` が `v` で始まれば「タグのソースが GitHub にある」とみなし
  （`VEIL_PORT_LOCAL_SRC=0`）、port の `DISTFILES` のとおり GitHub のタグの tarball を取る。
- port は Cargo.toml の版（0.9.2）でタグ `v0.9.2` のソースを指す。rc のタグ（`v0.9.2-rc.1`）も `v` で始まるので
  手元の木から tarball を作らず、まだ無い `v0.9.2` を取りに行く。
- port の job は continue-on-error なので draft は止まらないが、rc で port を確かめられない（本番のタグで初めて分かる）。

## 修正の方針

- 手元の木から tarball を作る条件を「タグの版（v を除く）が Cargo.toml の版と一致し、`-` を含まない」ときだけ
  GitHub から取る、に変える（rc・ブランチの ref は `VEIL_PORT_LOCAL_SRC=1`）。

## 試験

- 次の rc で port の試験 3 つが `make makesum` を通ること。

## 実施内容（2026-10-25）

- 原因: VM の中で `git describe --tags --exact-match` が取れず（VM へ同期した木にタグが無い）、Cargo.toml の版のタグ（v0.9.2）に
  落ちていた。一方 `VEIL_PORT_LOCAL_SRC` は `inputs.ref` が `v` で始まれば 0 だったので、まだ無い `v0.9.2` を取りに行った。
- dist.yml の `bsd-ports` で、runner 側の step（id `src`）が「ref が `v<Cargo.toml の版>` に一致し、版に `-` を含まない」ときだけ
  `local=0`、それ以外（rc のタグ・ブランチ）は checkout した木から `src.tar.gz` を作って `local=1` にし、`VEIL_PORT_LOCAL_SRC` に渡す。
- VM の中の port のタグは常に `v<Cargo.toml の版>`（`git describe` に頼らない）。
- 試験: v0.10.0-rc.1 の run で port の試験 3 つが `make makesum` を通ることを確かめる。
