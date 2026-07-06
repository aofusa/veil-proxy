#!/bin/sh
# veil パッケージ共通 prerm / %preun（.deb / .rpm 兼用）
set -e

# deb: remove|upgrade|deconfigure / rpm: 0 (final uninstall)
case "${1:-}" in
    remove|upgrade|deconfigure|0)
        if command -v systemctl >/dev/null 2>&1; then
            systemctl stop veil.service 2>/dev/null || true
            systemctl disable veil.service 2>/dev/null || true
        fi
        ;;
esac

exit 0