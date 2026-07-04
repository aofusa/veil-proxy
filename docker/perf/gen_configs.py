#!/usr/bin/env python3
"""Veil パフォーマンス計測用の config.toml バリアントを生成する。

full features ビルドのイメージ (veil:glibc / veil:musl) に対し、
ktls / http2 / reuseport balancing / open_file_cache が効く設定のみを組み合わせる。
どのバリアントも同一の静的ファイル (/var/www/index.html) を File アクションで配信する。
"""
import os

OUT = os.path.join(os.path.dirname(__file__), "configs")

def cfg(http2: bool, ktls: bool, lb: str, ofc: bool) -> str:
    ofc_section = ""
    if ofc:
        ofc_section = (
            "[route.open_file_cache]\n"
            "enabled = true\n"
            "valid_duration_secs = 60\n"
            "max_entries = 10000\n"
        )
    return f"""# 自動生成: Veil パフォーマンス計測用バリアント
# http2={http2} ktls={ktls} lb={lb} open_file_cache={ofc}
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = {str(http2).lower()}
threads = 0

[logging]
# アクセスログ (INFO) はホットパスのオーバーヘッドになるため計測中は抑止（nginx も access_log off で公平化）
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "{lb}"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = {str(ktls).lower()}
ktls_fallback_enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
{ofc_section}"""

# バリアント定義: (名前, http2, ktls, lb, ofc)
VARIANTS = [
    ("base",        True,  True,  "cbpf",   False),
    ("no_ktls",     True,  False, "cbpf",   False),
    ("no_http2",    False, True,  "cbpf",   False),
    ("kernel_lb",   True,  True,  "kernel", False),
    ("ofc",         True,  True,  "cbpf",   True),
    ("no_ktls_ofc", True,  False, "cbpf",   True),
]

def main():
    os.makedirs(OUT, exist_ok=True)
    for name, h2, ktls, lb, ofc in VARIANTS:
        path = os.path.join(OUT, f"{name}.toml")
        with open(path, "w") as f:
            f.write(cfg(h2, ktls, lb, ofc))
        print("wrote", path)

if __name__ == "__main__":
    main()
