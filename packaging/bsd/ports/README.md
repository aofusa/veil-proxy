# BSD ports for veil (F-253)

Templates of the port of veil for **FreeBSD ports** (`www/veil`), **OpenBSD ports** (`www/veil`) and **NetBSD
pkgsrc** (`www/veil`). Until they are in the official trees, use the generated port directories attached to each
GitHub release (`veil-<version>-bsd-ports.tar.gz`), or generate them yourself:

```sh
python3 tools/release/bsd-ports.py v0.9.0 /tmp/veil-ports
```

Each port builds veil from the release tarball with the OS's feature set (`full-freebsd`, `full-openbsd`,
`full-netbsd`) and installs the default configuration and page, which veil reads without options:

| OS | Configuration | Page | Service | Sandbox |
|---|---|---|---|---|
| FreeBSD | `/usr/local/etc/veil/config.toml` | `/usr/local/share/veil/www/` | `sysrc veil_enable=YES && service veil start` | drops to `www`, capsicum |
| OpenBSD | `/etc/veil/config.toml` | `/usr/local/share/veil/www/` | `rcctl enable veil && rcctl start veil` | drops to `_veil`, pledge / unveil |
| NetBSD | `/usr/pkg/etc/veil/config.toml` | `/usr/pkg/share/veil/www/` | `veil=YES` in `/etc/rc.conf`, `service veil start` | drops to `veil` (PaX MPROTECT is off for the binary: wasmtime) |

The default configuration listens on `127.0.0.1:8080` (plaintext HTTP/1.1 and h2c). On FreeBSD and NetBSD the
default path is built into the binary (`VEIL_DEFAULT_CONFIG_PATH`), so `veil` and `veil -t` without `-c` use it.

## Testing

`tools/release/bsd-port-test.sh` builds a port with each OS's ports framework in a VM (dependencies from binary
packages), runs `make makesum` (which writes `distinfo`), installs the package, starts the service and checks
`http://127.0.0.1:8080/`. The release workflow runs it for every release (`dist.yml`, job `bsd-ports`).

## Submitting

- FreeBSD: a new-port report in Bugzilla (Ports & Packages) with the port directory and `distinfo`.
- OpenBSD: a tarball of the port to ports@openbsd.org. Register the `_veil` user and group in
  `infrastructure/db/user.list` (numbers are given in order; the template uses 906, the next one in October 2026. Use the next free one when submitting).
- NetBSD: pkgsrc-wip first (`wip/veil`), then pkgsrc.
