#!/bin/sh
# veil パッケージ共通 postinst / %post（.deb / .rpm 兼用。F-251）
#
# 既定の設定（/etc/veil/config.toml）とページ（/usr/share/veil/www/）はパッケージのファイル（deb の conffile・
# rpm の %config(noreplace)）として入るので、ここでは作らない。自己署名の証明書も作らない（既定の設定は TLS を使わない）。
#
#   deb: postinst configure <前の版（新規なら空）>
#   rpm: %post <入っている数（新規 1・上げる 2）>
set -e

PKG_KIND="${VEIL_PKG_KIND:-deb}"
ACTION="${1:-}"
FROM_VERSION="${2:-}"

case "$ACTION" in
    configure|1|2) ;;
    *) exit 0 ;;
esac

if ! getent group veil >/dev/null 2>&1; then
    groupadd --system veil 2>/dev/null || addgroup --system veil 2>/dev/null || true
fi
if ! getent passwd veil >/dev/null 2>&1; then
    useradd --system --gid veil --home-dir /nonexistent --shell /usr/sbin/nologin veil 2>/dev/null \
        || adduser --system --ingroup veil --home /nonexistent --no-create-home --shell /usr/sbin/nologin veil 2>/dev/null \
        || true
fi

install -d -m 0750 -o veil -g veil /var/log/veil /var/cache/veil /var/tmp/veil
# 新しく入れたときだけ、設定を root:veil 0640 にする（降格後の veil が読める・他の利用者は読めない）
if [ "$ACTION" = 1 ] || { [ "$ACTION" = configure ] && [ -z "$FROM_VERSION" ]; }; then
    if [ -f /etc/veil/config.toml ]; then
        chown root:veil /etc/veil/config.toml
        chmod 0640 /etc/veil/config.toml
    fi
fi

# 前の版（0.8.x 以前）は設定を /var/etc/veil/config.toml に置いていた。その利用者はそのまま使い続ける
# （黙って移さない）。ユニットは EnvironmentFile の VEIL_CONFIG を読み、既定のパスより優先される。
if [ "$PKG_KIND" = rpm ]; then ENV_FILE=/etc/sysconfig/veil; else ENV_FILE=/etc/default/veil; fi
if [ -f /var/etc/veil/config.toml ] && [ ! -e "$ENV_FILE" ]; then
    mkdir -p "$(dirname "$ENV_FILE")"
    cat > "$ENV_FILE" <<'ENV'
# Written by the veil package: an earlier version kept its configuration in /var/etc/veil.
# veil keeps using it. To move to the default path, copy your settings into /etc/veil/config.toml
# and delete this file, then run: systemctl restart veil
VEIL_CONFIG=/var/etc/veil/config.toml
ENV
    echo "veil: keeping your configuration at /var/etc/veil/config.toml (see $ENV_FILE)" >&2
fi

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
    if [ "$PKG_KIND" = deb ]; then
        if [ -z "$FROM_VERSION" ]; then
            # Debian の慣習: 入れたら有効にして起動する（設定は既定の 127.0.0.1:8080 だけを待ち受ける）
            systemctl enable veil.service >/dev/null 2>&1 || true
            systemctl start veil.service || echo "veil: the service did not start; see: journalctl -u veil" >&2
        else
            systemctl try-restart veil.service || true
        fi
    else
        # Fedora・RHEL の方針: 入れただけでは起動しない（systemctl enable --now veil）。上げたときは動いていれば再起動
        [ "$ACTION" = 2 ] && { systemctl try-restart veil.service || true; }
    fi
fi

exit 0
