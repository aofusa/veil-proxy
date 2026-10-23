#!/bin/sh
# veil パッケージ共通 prerm / %preun（.deb / .rpm 兼用。F-251）
#
#   deb: prerm remove|upgrade|deconfigure|failed-upgrade   rpm: %preun <残る数（消す 0・上げる 1）>
# 上げるとき（deb の upgrade・rpm の 1）は止めない・無効にしない（postinst が try-restart する）。
set -e

case "${1:-}" in
    remove|deconfigure|0)
        if command -v systemctl >/dev/null 2>&1; then
            systemctl stop veil.service 2>/dev/null || true
            systemctl disable veil.service 2>/dev/null || true
        fi
        ;;
esac

exit 0
