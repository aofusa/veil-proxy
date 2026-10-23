# F-251: dnf のリポジトリを足す手順（文書と同じ。test-package.sh --repo が VEIL_REPO_URL を置き換えて使う）
set -e
curl -fsSL "VEIL_REPO_URL/veil.repo" -o /etc/yum.repos.d/veil.repo
dnf makecache -q --repo veil
