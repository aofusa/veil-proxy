#!/bin/sh
# veil パッケージ共通 postinst（.deb / .rpm 兼用）
set -e

# deb: configure / rpm: 1 (install) or 2 (upgrade)
case "${1:-}" in
    configure|1|2)
        ;;
    *)
        exit 0
        ;;
esac

if ! getent group veil >/dev/null 2>&1; then
    groupadd --system veil 2>/dev/null || addgroup --system veil 2>/dev/null || true
fi

if ! getent passwd veil >/dev/null 2>&1; then
    useradd --system --gid veil --home-dir /nonexistent --shell /sbin/nologin veil 2>/dev/null \
        || useradd -r -g veil -d /nonexistent -s /sbin/nologin veil 2>/dev/null \
        || adduser --system --group --home /nonexistent --shell /sbin/nologin veil 2>/dev/null \
        || true
fi

install -d -m 0750 -o veil -g veil /var/log/veil /var/cache/veil /var/tmp/veil
install -d -m 0750 -o root -g veil /var/etc/veil /var/etc/veil/ssl

WWW_CREATED=0
if [ ! -d /var/www ]; then
    mkdir -p /var/www
    WWW_CREATED=1
fi

if [ ! -f /var/etc/veil/config.toml ]; then
    install -m 0640 -o root -g veil /usr/share/veil/config.toml.default /var/etc/veil/config.toml
fi

if [ "$WWW_CREATED" = "1" ] && [ ! -f /var/www/index.html ]; then
    install -m 0644 -o veil -g veil /usr/share/veil/www/index.html /var/www/index.html
fi

if [ ! -f /var/etc/veil/ssl/cert.pem ] || [ ! -f /var/etc/veil/ssl/key.pem ]; then
    openssl req -x509 -newkey EC -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
        -keyout /var/etc/veil/ssl/key.pem \
        -out /var/etc/veil/ssl/cert.pem \
        -days 3650 \
        -subj "/CN=localhost" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
        -addext "basicConstraints=critical,CA:FALSE" \
        -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
        -addext "extendedKeyUsage=serverAuth"
    chown veil:veil /var/etc/veil/ssl/key.pem /var/etc/veil/ssl/cert.pem
    chmod 600 /var/etc/veil/ssl/key.pem
    chmod 644 /var/etc/veil/ssl/cert.pem
fi

chown -R veil:veil /var/log/veil /var/cache/veil /var/tmp/veil
chown -R root:veil /var/etc/veil
chown veil:veil /var/etc/veil/ssl/key.pem
chmod 0750 /var/etc/veil
chmod 0750 /var/etc/veil/ssl
chmod 0640 /var/etc/veil/config.toml
chmod 600 /var/etc/veil/ssl/key.pem

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi

exit 0