# Переходный период: Rust-делегаты без статического IP + legacy-делегаты со статическим IP

## Кто с кем и как разговаривает

| Направление | Транспорт | Нужен ли статический IP |
|---|---|---|
| Rust → legacy (блоки `postBlock`, транзакции `postTransactions`) | исходящее WS на `ip:4001` legacy-узла | **нет** - нода сама открывает соединение |
| legacy → Rust (legacy тянет блоки/пиров) | legacy умеет только `ip:4001` | **да** - только gateway-узлы |
| Rust ↔ Rust (блоки, транзакции, репутация пиров, RPC GetBlocks) | iroh (QUIC, hole-punch, relay) | **нет** |
| Кошельки / эксплореры / dApps → Rust REST 4003 | netfory-provider `api://<NodeId>/<name>` | **нет** |

`netfory-provider` - это только мост «Web 4.0 → REST/WS нашей ноды». Он **не участвует в консенсусе** и на legacy-серверах для связи делегатов не нужен: legacy core не умеет ходить через iroh/`api://`.

## 1. Rust-делегат на динамическом IP (домашний ПК, NAT)

`node.yaml`:
```yaml
db_path: ./data
sync:
  bootstrap_snapshot: latest          # первый старт: снапшот вместо долгой синхронизации
p2p:
  legacy_enabled: true                # держим связь со старой сетью (порт 4001 наружу)
  parallel_peers: 4
  relay_fanout: 3
  iroh:
    enabled: true
    secret_key_file: ./iroh.key
    bootstrap: ["<EndpointId gateway-узла>", "<EndpointId другого Rust-узла>"]
    relay: true                       # NAT-обход через relay/hole-punch
delegate:
  enabled: true
  secrets: ["<passphrase делегата>"]  # или secrets_file: ./delegates.json (legacy-формат)
  broadcast_fanout: 8                 # скольким legacy-пирам пушим свой блок
  quorum_share: 0.5                   # доля пиров, подтвердивших наш tip, перед форжингом
```
Запуск: `sth-core run`. В консоли должно появиться `Loaded 1 active delegate: <name> (<pk>)`, при своём слоте -
`Slot … belongs to delegate …`, `Forged new block …`, `Broadcasting block … to N peers (X accepted)`.
`X accepted > 0` = legacy-узлы приняли блок. Контроль: `http://127.0.0.1:4003/status` и `/api/node/forging`.
Порты наружу открывать не нужно.

## 2. Gateway-узел (VPS со статическим IP, 1–3 штуки на сеть)

Тот же sth-core, дополнительно:
```yaml
p2p:
  legacy_listen: "0.0.0.0:4001"       # входящий legacy-порт - старые узлы видят нас как пир v3.8.2
  legacy_public_addr: "203.0.113.10:4001"   # анонсируется Rust-узлам через iroh (автодискавери gateway)
  iroh:
    enabled: true
    relay: true
```
Откройте в файрволе UDP/TCP 4001 (входящий). `sth-core iroh-id` печатает EndpointId - его раздайте
остальным Rust-узлам в `p2p.iroh.bootstrap`. Legacy-узлы добавят gateway в свой список пиров сами
(после первого обращения от нас или через `peers.json`).

## 3. Legacy-делегаты (static IP) - ничего менять не нужно

Они продолжают получать блоки push'ем от Rust-делегатов и могут тянуть блоки с gateway-узлов.
netfory-provider рядом с legacy-узлом (`local_url: http://localhost:4003/api`) лишь публикует их REST
в Web 4.0 - полезно кошелькам, к консенсусу отношения не имеет.

## 4. netfory-provider рядом с Rust-нодой (опционально)

```yaml
endpoints:
  smartholdem-node:
    protocol: api
    name: node-rs1
    local_url: http://127.0.0.1:4003
    rate_limit_per_peer: 500
    local_ws_url: ws://127.0.0.1:4001      # только если включён p2p.legacy_listen
    ws_rate_limit_per_peer: 1000
```
Так REST (и legacy-протокол поверх WS) становятся доступны из Web 4.0 без статического IP; другие
Rust-узлы могут указать такой пир в `p2p.legacy_peers` как `ws://…` URL.

## 5. Проверка

1. `sth-core peers` - таблица legacy-пиров (latency/height).
2. `curl 127.0.0.1:4003/api/node/peers` - `meta.gateways`, iroh-пиры, состояние банов.
3. `curl 127.0.0.1:4003/api/node/forging` - делегаты, следующий слот, последний блок, причины пропусков.
4. При старте: `Your NTP connectivity has been verified … Local clock is off by Nms` - дрейф > 1с чинить обязательно.
