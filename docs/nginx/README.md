# nginx in front of a public sth-core node

Example for `node-pq01.smartholdem.io`: files `node-pq01.smartholdem.io.conf` and `snippets/sth-cors.conf`.

## Layout

| URL                                   | upstream          | purpose                                                    |
|---------------------------------------|-------------------|------------------------------------------------------------|
| `https://node-pq01.smartholdem.io/`   | `127.0.0.1:4888`  | operator metrics page (`api.page_metrics`)                 |
| `.../api/ntfry/*`                     | `127.0.0.1:4888`  | peers / delegates / metrics / finality / tokens / market   |
| `.../api/*`, `.../status`             | `127.0.0.1:4003`  | full REST API, including `POST /api/transactions`         |
| `wss://.../ws/events`, `wss://.../ws/api` | `127.0.0.1:4005` | WebSocket bridge for browser wallets (SHIP-44) (`snippets/sth-ws-bridge.conf` on the same domain, `ws-bridge.smartholdem.io.conf` on a separate domain; wallet instructions: `../wallet/WALLET-WS-MSGPACK_RU.md`) |

The node listens only on loopback; only nginx (443) and legacy P2P (4001) are exposed externally, if the node acts as a gateway.

```yaml
# node.yaml
api:
  host: 127.0.0.1
  port: 4003
  page_metrics: true
  metrics_listen: "127.0.0.1:4888"
  cors_origins: []          # CORS is handled by nginx; disabled in the node to avoid duplicate headers
```

## Installation (Debian/Ubuntu)

```bash
apt install nginx certbot
mkdir -p /var/www/letsencrypt
cp docs/nginx/node-pq01.smartholdem.io.conf /etc/nginx/sites-available/
cp docs/nginx/snippets/sth-cors.conf        /etc/nginx/snippets/
ln -s /etc/nginx/sites-available/node-pq01.smartholdem.io.conf /etc/nginx/sites-enabled/
rm -f /etc/nginx/sites-enabled/default

# first run without a certificate: temporarily comment out the 443 server block, then
nginx -t && systemctl reload nginx
certbot certonly --webroot -w /var/www/letsencrypt -d node-pq01.smartholdem.io
# restore the 443 block
nginx -t && systemctl reload nginx
```

`certbot renew` extends the certificate using the same `/.well-known/acme-challenge/` on port 80.

## Verification

```bash
curl -sI https://node-pq01.smartholdem.io/api/node/status | grep -iE "^HTTP|access-control"
curl -sI -X OPTIONS -H "Origin: http://localhost:3000" -H "Access-Control-Request-Method: POST" \
     https://node-pq01.smartholdem.io/api/transactions          # expect 204 + Access-Control-Allow-*
curl -s https://node-pq01.smartholdem.io/api/ntfry/metrics | head -c 300
```

## Restrict CORS to a list of domains

Instead of `"*"` in `snippets/sth-cors.conf`, use a `map` in the `http` context (at the start of the `.conf` file) and a variable:

```nginx
map $http_origin $sth_cors {
    default "";
    "~^https://(wallet|explorer)\.smartholdem\.io$" $http_origin;
    "http://localhost:3000"                          $http_origin;
}
```
```nginx
# snippets/sth-cors.conf
add_header Access-Control-Allow-Origin $sth_cors always;
add_header Vary Origin always;
...
```

Alternative without nginx CORS: remove `include snippets/sth-cors.conf` and enable it in `node.yaml` with `cors_origins: ["https://wallet.smartholdem.io"]` - the node will emit the headers itself (see `tests/cors.rs`).

## Limits

- `sth_api` - 20 r/s per IP (burst 40–60) for reads; `sth_post` - 2 r/s (burst 10) on `/api/transactions`.
- `client_max_body_size 1m` - sufficient for multipay with up to 1024 recipients.
- If you see 429/503 in the logs, increase the `rate`/`burst`, or move heavy explorers to a separate node.
