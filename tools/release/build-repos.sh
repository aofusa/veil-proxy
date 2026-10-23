#!/usr/bin/env bash
# F-251: apt・dnf のリポジトリ（静的なファイル）を作り直して署名する。
#
#   tools/release/build-repos.sh <repo-dir> <new-packages-dir> <gpg-key-id> [keep=3]
#
# <repo-dir> は公開するリポジトリの木（aofusa/veil-packages の Pages の中身）。<new-packages-dir> の veil_*.deb・
# veil-*.rpm を足し、版の新しいものから keep 個だけ残して、索引を作り直して署名する（GitHub Pages の 1GB の上限）。
#
#   <repo-dir>/veil.gpg                       公開鍵（利用者が /etc/apt/keyrings に置く）
#   <repo-dir>/apt/pool/main/v/veil/*.deb
#   <repo-dir>/apt/dists/stable/{InRelease,Release,Release.gpg,main/binary-{amd64,arm64}/Packages{,.gz}}
#   <repo-dir>/rpm/{x86_64,aarch64}/*.rpm, <repo-dir>/rpm/repodata/（repomd.xml.asc を付ける）, <repo-dir>/veil.repo
#
# 要るもの: apt-ftparchive（apt-utils）、createrepo_c、rpm（rpmsign）、gpg（鍵は gpg の鍵束に入れておく）。
set -euo pipefail
REPO="${1:?repo-dir}"; NEW="${2:?new packages dir}"; KEY="${3:?gpg key id}"; KEEP="${4:-3}"
BASE_URL="${VEIL_REPO_URL:-https://aofusa.github.io/veil-packages}"

mkdir -p "$REPO/apt/pool/main/v/veil" "$REPO/rpm/x86_64" "$REPO/rpm/aarch64"
cp "$NEW"/veil_*.deb "$REPO/apt/pool/main/v/veil/"
for r in "$NEW"/veil-*.rpm; do
    case "$r" in *.x86_64.rpm) cp "$r" "$REPO/rpm/x86_64/" ;; *.aarch64.rpm) cp "$r" "$REPO/rpm/aarch64/" ;; esac
done

# 古い版を消す（版で並べて新しいものから KEEP 個）
prune() {
    local dir="$1" pat="$2" sedexpr="$3"
    local files vers
    files="$(ls "$dir" | grep -E "$pat" || true)"
    [ -n "$files" ] || return 0
    vers="$(sed -E "$sedexpr" <<<"$files" | sort -uV | tail -n "$KEEP")"
    for f in $files; do
        v="$(sed -E "$sedexpr" <<<"$f")"
        grep -qxF "$v" <<<"$vers" || rm -f "$dir/$f"
    done
}
prune "$REPO/apt/pool/main/v/veil" '^veil_.*\.deb$' 's/^veil_([^_]+)_.*/\1/'
prune "$REPO/rpm/x86_64" '^veil-.*\.rpm$' 's/^veil-([^-]+)-.*/\1/'
prune "$REPO/rpm/aarch64" '^veil-.*\.rpm$' 's/^veil-([^-]+)-.*/\1/'

# --- apt ---
(
    cd "$REPO/apt"
    for arch in amd64 arm64; do
        d="dists/stable/main/binary-$arch"; mkdir -p "$d"
        apt-ftparchive --arch "$arch" packages pool > "$d/Packages"
        gzip -9nc "$d/Packages" > "$d/Packages.gz"
    done
    apt-ftparchive \
        -o APT::FTPArchive::Release::Origin=veil \
        -o APT::FTPArchive::Release::Label=veil \
        -o APT::FTPArchive::Release::Suite=stable \
        -o APT::FTPArchive::Release::Codename=stable \
        -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
        -o APT::FTPArchive::Release::Components=main \
        -o APT::FTPArchive::Release::Description="veil packages" \
        release dists/stable > dists/stable/Release.new
    mv dists/stable/Release.new dists/stable/Release
    rm -f dists/stable/InRelease dists/stable/Release.gpg
    gpg --batch --yes --local-user "$KEY" --clearsign -o dists/stable/InRelease dists/stable/Release
    gpg --batch --yes --local-user "$KEY" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
)

# --- rpm ---
for r in "$REPO"/rpm/*/*.rpm; do
    # 署名済みのもの（前の版のパッケージ）は飛ばす
    if ! rpm -qpi "$r" 2>/dev/null | grep -qE '^Signature *:.*Key ID'; then
        rpmsign --addsign --define "_gpg_name $KEY" --define "__gpg $(command -v gpg)" \
            --define "_gpg_sign_cmd_extra_args --batch --pinentry-mode loopback" "$r" >/dev/null
    fi
done
createrepo_c --update -q "$REPO/rpm"
rm -f "$REPO/rpm/repodata/repomd.xml.asc"
gpg --batch --yes --local-user "$KEY" --armor --detach-sign -o "$REPO/rpm/repodata/repomd.xml.asc" "$REPO/rpm/repodata/repomd.xml"

# --- 利用者が使うファイル ---
gpg --batch --yes --export "$KEY" > "$REPO/veil.gpg"
gpg --batch --yes --armor --export "$KEY" > "$REPO/veil.asc"
cat > "$REPO/veil.repo" <<R
[veil]
name=veil
baseurl=$BASE_URL/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$BASE_URL/veil.asc
R
cat > "$REPO/index.html" <<H
<!doctype html><meta charset="utf-8"><title>veil packages</title>
<h1>veil packages</h1>
<p>Signed apt and dnf repositories for <a href="https://github.com/aofusa/veil-proxy">veil</a>.
Instructions: <a href="https://github.com/aofusa/veil-proxy/blob/main/docs/guide/running.md#apt-and-dnf-repositories">docs/guide/running.md</a>.</p>
<p>Signing key: <a href="veil.asc">veil.asc</a> (fingerprint: <code>$(gpg --batch --with-colons --fingerprint "$KEY" | awk -F: '/^fpr/ {print $10; exit}')</code>)</p>
H
touch "$REPO/.nojekyll"
echo "repositories updated in $REPO"
