# cmux-proxy

Header-based reverse proxy that routes to different local ports based on the `X-Cmux-Port-Internal` header. Supports:

- HTTP requests (streaming)
- WebSocket upgrades (transparent tunneling)
- Generic TCP via HTTP CONNECT tunneling

This is useful for multiplexing multiple local services behind a single port while choosing the target by header.

## Build and Run

- Build: `cargo build --release`
- Run: `./target/release/cmux-proxy --listen 0.0.0.0:8080` (default)

Env/flags:

- `--listen` or `CMUX_LISTEN` (accepts multiple or comma-separated). Defaults to `0.0.0.0:8080,127.0.0.1:8080`.
  - Note: binding to `0.0.0.0:<port>` already covers `127.0.0.1:<port>`; duplicate binds are deduped to avoid conflicts.
- `--upstream-host` or `CMUX_UPSTREAM_HOST` (default `127.0.0.1`)

## Usage

- HTTP
  - `curl -v -H 'X-Cmux-Port-Internal: 3000' http://127.0.0.1:8080/api`
  - Proxies to `http://127.0.0.1:3000/api`.

- WebSocket (client must send the header)
  - Example with websocat: `websocat -H 'X-Cmux-Port-Internal: 3001' ws://127.0.0.1:8080/ws`
  - Proxies to `ws://127.0.0.1:3001/ws` (upgrade tunneled).

- TCP via CONNECT (create a raw TCP tunnel)
  - The proxy will ignore the CONNECT target host/port and use the header port.
  - Example (Redis tunnel): `curl --http1.1 -x http://127.0.0.1:8080 -H 'X-Cmux-Port-Internal: 6379' -v https://example` (establishes CONNECT then tunnels). A better test is to script a `CONNECT` request with `nc`.

## Notes

- The header `X-Cmux-Port-Internal` is required on every request; value must be a valid TCP port (1-65535).
- Only HTTP/1.1 is supported on the front-end. HTTP/2 is not supported (WebSocket over H2 is not handled).
- Hop-by-hop headers are stripped where appropriate; upgrade is handled specially to preserve handshake headers.
- Upstream host defaults to `127.0.0.1`. If you need another host, pass `--upstream-host`. The header only specifies the port.

## Caveats

- This proxy does not terminate TLS; inbound must be plain HTTP/WS. If you need TLS, put a TLS terminator in front.
- For CONNECT, the client and upstream protocols are opaque to the proxy. The proxy just tunnels bytes.

## License

MIT
