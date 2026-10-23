# aofusa/homebrew-tap

Homebrew formulae for [veil](https://github.com/aofusa/veil-proxy), a high-performance reverse proxy and web
server (HTTP/1.1, HTTP/2, HTTP/3).

```sh
brew install aofusa/tap/veil
brew services start veil        # or just: veil
curl http://127.0.0.1:8080/
```

The formula installs a default configuration at `$(brew --prefix)/etc/veil/config.toml` that serves
`$(brew --prefix)/share/veil/www` on `127.0.0.1:8080`. Upgrades keep your changes to the configuration.

This tap is updated automatically when veil is released (the formula is generated in veil-proxy; open
issues there: <https://github.com/aofusa/veil-proxy/issues>).
