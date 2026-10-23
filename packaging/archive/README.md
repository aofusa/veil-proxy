# veil @VERSION@ (@TARGET@)

This archive contains the `veil` binary only. It does **not** install a configuration file or a web page:
create them yourself. (The packages for deb / rpm, Homebrew, winget and the BSD ports, and the container
image, do install a default configuration and page.)

## Try the example

```sh
cd veil-@VERSION@-@TARGET@
./veil -c examples/config.toml        # Windows: .\veil.exe -c examples\config.toml
# open http://127.0.0.1:8080/
```

`examples/config.toml` serves `examples/www/` on `127.0.0.1:8080` (plaintext HTTP/1.1 and h2c) and logs every
request to standard output. Its paths are relative, so run it from this directory.

## Use it for real

1. Copy `examples/config.toml` and a `www` directory where you want, and change the paths in the file to
   absolute paths.
2. Start veil with one of:
   - `veil -c /path/to/config.toml`
   - `VEIL_CONFIG=/path/to/config.toml veil`
   - put the file at the default path and run `veil` (`veil --help` shows the default path:
     `/etc/veil/config.toml` on unix, `%ProgramData%\veil\config.toml` on Windows).
3. Check a change with `veil -t -c FILE` and reload without downtime with SIGHUP.

Without a configuration file veil stops with an error that names the path it looked for.

- Configuration file: https://github.com/aofusa/veil-proxy/blob/main/docs/guide/running.md#configuration-file
- Every key: https://github.com/aofusa/veil-proxy/blob/main/examples/config.toml
- Guides: https://github.com/aofusa/veil-proxy/blob/main/docs/guide/README.md

Licensed under either of Apache License 2.0 (`LICENSE-APACHE`) or MIT (`LICENSE-MIT`) at your option.
