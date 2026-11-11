# nginx перед публичной нодой sth-core

Пример для `node-pq01.smartholdem.io`: файлы `node-pq01.smartholdem.io.conf` и `snippets/sth-cors.conf`.

## Схема

| URL                                   | upstream          | что это                                                  |
|---------------------------------------|-------------------|----------------------------------------------------------|
| `https://node-pq01.smartholdem.io/`   | `127.0.0.1:4888`  | страница метрик оператора (`api.page_metrics`)           |
| `.../api/ntfry/*`                       | `127.0.0.1:4888`  | peers / delegates / metrics / finality / tokens / market |
| `.../api/*`, `.../status`                 | `127.0.0.1:4003`  | полный REST API, `POST /api/transactions`                |
| `wss://.../ws/events`, `wss://.../ws/api` | `127.0.0.1:4005`  | WebSocket-мост SHIP-44 для браузерных кошельков (`snippets/sth-ws-bridge.conf` - на том же домене, `ws-bridge.smartholdem.io.conf` - отдельный домен; инструкция для кошелька - `../wallet/WALLET-WS-MSGPACK_RU.md`) |

Нода слушает только loopback - наружу открыт лишь nginx (443) и legacy P2P (4001), если нода - gateway.

```yaml
# node.yaml
api:
  host: 127.0.0.1
  port: 4003
  page_metrics: true
  metrics_listen: "127.0.0.1:4888"
  cors_origins: []          # CORS выдаёт nginx; в ноде выключен, чтобы заголовки не дублировались
```

## Установка (Debian/Ubuntu)

```bash
apt install nginx certbot
mkdir -p /var/www/letsencrypt
cp docs/nginx/node-pq01.smartholdem.io.conf /etc/nginx/sites-available/
cp docs/nginx/snippets/sth-cors.conf        /etc/nginx/snippets/
ln -s /etc/nginx/sites-available/node-pq01.smartholdem.io.conf /etc/nginx/sites-enabled/
rm -f /etc/nginx/sites-enabled/default

# первый запуск без сертификата: временно закомментируйте блок server 443, затем
nginx -t && systemctl reload nginx
certbot certonly --webroot -w /var/www/letsencrypt -d node-pq01.smartholdem.io
# вернуть блок 443
nginx -t && systemctl reload nginx
```

`certbot renew` продлевает сертификат через тот же `/.well-known/acme-challenge/` на 80-м порту.

## Проверка

```bash
curl -sI https://node-pq01.smartholdem.io/api/node/status | grep -iE "^HTTP|access-control"
curl -sI -X OPTIONS -H "Origin: http://localhost:3000" -H "Access-Control-Request-Method: POST" \
     https://node-pq01.smartholdem.io/api/transactions          # ожидаем 204 + Access-Control-Allow-*
curl -s https://node-pq01.smartholdem.io/api/ntfry/metrics | head -c 300
```

## Ограничить CORS списком доменов

Вместо `"*"` в `snippets/sth-cors.conf` - `map` в контексте `http` (в начало `.conf`-файла) и переменная:

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

Альтернатива без nginx-CORS: убрать `include snippets/sth-cors.conf` и включить в `node.yaml`
`cors_origins: ["https://wallet.smartholdem.io"]` - нода выдаст заголовки сама (см. `tests/cors.rs`).

## Лимиты

- `sth_api` - 20 r/s на IP (burst 40–60) для чтения; `sth_post` - 2 r/s (burst 10) на `/api/transactions`.
- `client_max_body_size 1m` - хватает для multipay на 1024 получателей.
- При 429/503 в логах поднимите `rate`/`burst`, либо вынесите тяжёлые эксплореры на отдельную ноду.
