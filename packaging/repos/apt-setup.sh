# F-251: apt のリポジトリを足す手順（文書と同じ。test-package.sh --repo が VEIL_REPO_URL を置き換えて使う）
set -e
install -d -m 0755 /etc/apt/keyrings
curl -fsSL "VEIL_REPO_URL/veil.gpg" -o /etc/apt/keyrings/veil.gpg
echo "deb [signed-by=/etc/apt/keyrings/veil.gpg] VEIL_REPO_URL/apt stable main" > /etc/apt/sources.list.d/veil.list
apt-get update -qq
