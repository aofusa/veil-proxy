#!/usr/bin/env bash
# F-254: Helm chart の試験と公開に使う helm・kind・kubeconform を、版とハッシュを固定して入れる（linux/amd64 の runner）。
#
#   tools/release/install-k8s-tools.sh <bin-dir>
set -euo pipefail
BIN="${1:?bin dir}"; mkdir -p "$BIN"
W="$(mktemp -d)"; cd "$W"
fetch() { curl -fsSL -o "$2" "$1"; echo "$3  $2" | sha256sum -c - >/dev/null || { echo "checksum mismatch: $1" >&2; exit 1; }; }
fetch https://get.helm.sh/helm-v3.19.0-linux-amd64.tar.gz helm.tgz a7f81ce08007091b86d8bd696eb4d86b8d0f2e1b9f6c714be62f82f96a594496
tar xzf helm.tgz && install -m 0755 linux-amd64/helm "$BIN/helm"
fetch https://github.com/kubernetes-sigs/kind/releases/download/v0.30.0/kind-linux-amd64 kind 517ab7fc89ddeed5fa65abf71530d90648d9638ef0c4cde22c2c11f8097b8889
install -m 0755 kind "$BIN/kind"
fetch https://github.com/yannh/kubeconform/releases/download/v0.7.0/kubeconform-linux-amd64.tar.gz kc.tgz c31518ddd122663b3f3aa874cfe8178cb0988de944f29c74a0b9260920d115d3
tar xzf kc.tgz kubeconform && install -m 0755 kubeconform "$BIN/kubeconform"
rm -rf "$W"
"$BIN/helm" version --short; "$BIN/kind" version; "$BIN/kubeconform" -v
