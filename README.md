# sth-core-rust

Author: TechnoL0g

Rust rewrite of the SmartHoldem (`@smartholdem/core`, network byte `63`) relay node.
Current status: **(crypto + models, Sled storage, legacy HTTP sync).**

## Layout

```
src/
  config.rs              mainnet constants (pubKeyHash 63, epoch 2023-08-29, milestones)
  models/                Block / Transaction - JSON identical to core IBlockData / ITransactionData
  crypto/
    hash.rs              sha256 / ripemd160
    address.rs           base58check addresses (network byte 63)
    keys.rs              KeyPair (sha256(passphrase) -> secp256k1)
    ecdsa.rs             DER ECDSA, strict encoding + low-S (block signatures)
    schnorr.rs           legacy bip-schnorr ("is-square" R) - v2 transaction signatures
    tx_serializer.rs     AIP-11 wire format, transaction id, signing hash
    block_serializer.rs  header serialisation, block id, payload hash, full verify_block()
  storage.rs             Sled: b:<height>, bid:<id>, t:<txid>, w:<address>; atomic apply_block()
  node_pool.rs           per-node rate limiter (4 req/s, 250/60s window, 429 parking, failure backoff)
  sync.rs                batched legacy HTTP sync pipeline (reqwest + indicatif), resumable
  snapshot.rs            core-snapshots dump import (gzip records, msgpack), download latest .tgz
  crypto/tx_deserializer.rs, crypto/block_deserializer.rs  wire -> struct (inverse of the serialisers)
  api/                   axum REST API (legacy JSON): mod (router, pagination), render, node, blocks,
                         transactions, wallets, delegates, locks      mempool.rs: tx pool + relay + events
  p2p_iroh/              Web 4.0 layer: mod (endpoint/router), proto (JSON messages), rpc (sth/rpc/1 ALPN:
                         GetStatus/GetBlocks), gossip (blocks + transactions topics), peers (table)
  delegate/              forging module: round (rounds/slots/shuffle), block_builder (assemble + sign),
                         forger (slot loop + postBlock broadcast), round tracker
  network/mainnet/       network.json, milestones.json, exceptions.json, genesisBlock.json.gz (crypto-networks layout)
  p2p_legacy/            legacy inter-node protocol (port 4001)
    proto.rs             hand-written prost messages           client.rs  nes framing + LegacyPeer
    health.rs            peer table (latency / height / bans)  follow.rs  parallel catch-up + live follow
    relay.rs             postTransactions fan-out from the mempool
  genesis.rs             embedded mainnet genesis block (gzip JSON) - seeds an empty database
  node_config.rs         node.yaml (sections api / sync / p2p / rewards / mempool)
  node.rs                NodeContext: storage + mempool + peer table + API + intake, from NodeConfig
  main.rs                CLI: init | run | peers | peer-status | sync | snapshot | info | verify-block | import-block | wallet
tests/
  crypto_vectors.rs      real mainnet blocks / transactions (bit-exact ids + signatures)
  storage.rs             storage + state transition tests
```

## Build & test

```bash
cargo build --release
cargo test
```

## CLI

```bash
# fetch a raw block from a legacy node and verify it
curl -s "https://node0.smartholdem.io/api/blocks/<id>?transform=false" > block.json
sth-core verify-block block.json

# apply to the local Sled db and inspect
sth-core --db-path ./data import-block block.json
sth-core --db-path ./data info
sth-core --db-path ./data wallet SR1W4qS8DCPN65oV9Jd8JSLbfU5vhmEEky
```

## Sync (legacy HTTP bootstrap, rate-limit aware, temporary)

```bash
cargo build --release && cargo test
./target/release/sth-core --db-path ./data sync                 # catch up, then exit
./target/release/sth-core --db-path ./data sync --follow        # keep following the chain
./target/release/sth-core --db-path ./data sync --batch 50 --concurrency 6 --rps 4 --window-limit 250
./target/release/sth-core --db-path ./data sync --nodes https://node0.smartholdem.io,https://node3.smartholdem.io
./target/release/sth-core --db-path ./data sync --skip-verify  # linkage + ids only (fast, less safe)
RUST_LOG=debug ./target/release/sth-core sync --quiet           # no progress bar, logs only
```

Console:
```
Syncing: [=====>                              ] 11704000 / 11704428 (24 blocks/sec, ETA: 18s)
Current node: node2.smartholdem.io | Requests: 180/300
```
Ctrl+C stops after the current batch (state is committed per block); `sync` resumes from the Sled tip.

Rate limiting (`src/node_pool.rs`) - the legacy API allows 300 req / 60 s per IP (5 req/s):
* ≤ 4 req/s per node (20 % headroom), ≤ 250 req per 60 s sliding window per node;
* a node that hits its window is skipped until the window frees, requests spread over all 6 nodes
  (≈ 24 req/s aggregate);
* HTTP 429 -> `WARN Rate limit hit on node0..., switching to node1..., waiting 60s`, node parked 60 s;
* network error / timeout / 5xx -> backoff 1 s -> 2 s -> 4 s -> 8 s -> 16 s; 5 consecutive failures park the node 30 s;
* all nodes parked -> the pool waits for the earliest release, nothing is dropped.

Fetching: ranges of `--batch` blocks (default 100 = API max, halves the request count vs. 50) via
`GET /api/blocks?height.from=X&height.to=Y&limit=N&orderBy=height:asc&transform=false`; non-empty blocks get
their transactions via `GET /api/blocks/:id/transactions?transform=false` (paginated, sorted by `sequence`).
Every block is linked to the tip (`previousBlock == tip.id`), fully verified (`verify_block`: id, ECDSA
signature, payload hash, tx ids + schnorr signatures) and applied atomically. A rejected batch is re-fetched
once from another node, then the sync stops with an error.

Estimate: 11.7 M blocks / 100 ≈ 117 k requests (+1 per non-empty block) at ≈ 24 req/s ≈ 1.5 h.

> SmartHoldem nodes synchronise with each other over the dedicated P2P port (`p2p.blocks.getBlocks`,
> WebSocket + protobuf, up to 400 blocks per call), not the REST API. This HTTP path is a bootstrap
> shortcut and will be replaced by iroh.

## Snapshot bootstrap (fastest cold start)

Dumps made by the legacy node (`yarn sth snapshot:dump`, core-snapshots "default" codec) are imported
directly - no PostgreSQL needed. Format: `<start>-<end>/{meta.json, blocks, transactions, rounds}`, gzip
streams of `[u32 LE len][record]`; blocks = serialised headers, transactions = msgpack
`[id, blockId, blockHeight, sequence, timestamp, serialized]` (rounds are ignored by a relay).

```bash
sth-core snapshot download --out ./snapshots            # newest <start>-<end>.tgz from snapshots.smartholdem.io
sth-core snapshot info ./snapshots/1-11705253            # meta.json summary
sth-core --db-path ./data snapshot import ./snapshots/1-11705253.tgz --fast-import
sth-core --db-path ./data sync --from-dump latest --fast-import   # download -> import -> continue HTTP sync
sth-core --db-path ./data sync --from-dump /path/1-11705253 --fast-import --follow
```

* Every block is linked (`previousBlock == tip.id`), ids are recomputed from bytes, transaction ids are
  recomputed and payload hashes checked. Without `--fast-import` block ECDSA and transaction schnorr
  signatures are verified as well (slower; use for untrusted dumps).
* Import is resumable: blocks at or below the local height are skipped, Ctrl+C stops between 1000-block chunks.
* Blocks are written 1000 per sled transaction (`Storage::apply_blocks`) in a compact wire encoding
  (header bytes + serialised transactions, decoded to JSON on read).
* Measured on the real `1-11705253` dump (fast import, release build): ~28 000 blocks/sec -> full chain in ~7 min;
  on-disk ≈ 1 KB/block in sled (≈ 12 GB for 11.7 M blocks).

## Relay node: `init` + `run` (headless, node.yaml)

```bash
sth-core init                      # writes a commented node.yaml (rewards.reward_address / reward_passphrase, ...)
sth-core run                       # uses ./node.yaml when present: snapshot bootstrap (empty DB only) -> P2P catch-up -> follow + API
sth-core run --config /etc/sth/node.yaml --db-path /var/sth --no-api      # CLI flags override the file
sth-core run --from-dump ./1-11705253.tgz --fast-import                   # force a snapshot import first
sth-core run --no-p2p              # REST polling instead of the legacy P2P port
CORE_API_HOST=127.0.0.1 CORE_API_PORT=4003 sth-core run          # legacy env names are honoured
```

`node.yaml` sections map 1:1 to modules: `api` (host/port), `sync` (REST nodes, `bootstrap_snapshot: latest|<path>|""`,
`verify_blocks`), `p2p` (`legacy_enabled`, `legacy_peers`, `parallel_peers` = 4, `relay_fanout` = 3, `refresh_secs`,
`iroh_*` reserved), `rewards` (`reward_address` **or** `reward_passphrase` -> derived address, logged at start and kept
in `NodeContext.reward_address` for the upcoming delegate module), `mempool.max_size`.

The API binds to **127.0.0.1:4003** by default (local bridge for netfory-provider - never exposed publicly).
Responses use the legacy transformed JSON (`?transform=false` -> raw core objects); pagination meta is identical.

| Endpoint | Notes |
|---|---|
| `GET /api/blockchain`, `/api/node/status`, `/api/node/syncing`, `/api/node/configuration`, `/api/node/fees`, `/api/transactions/fees`, `/api/peers` | node info |
| `GET /api/blocks` (`page,limit,height,id,height.from,height.to,orderBy`), `/api/blocks/first`, `/api/blocks/last`, `/api/blocks/:idOrHeight`, `/api/blocks/:id/transactions` | blocks |
| `GET /api/transactions` (`type,typeGroup,senderId,recipientId,address,blockId`), `/api/transactions/:id`, `/api/transactions/unconfirmed[/:id]` | transactions |
| `POST /api/transactions` `{ "transactions": [...] }` -> `{ data: { accept, broadcast, excess, invalid }, errors }` | mempool: id, schnorr signature, network, nonce (+pending), balance, recipients; accepted txs are relayed to a legacy node |
| `GET /api/wallets`, `/api/wallets/:addr|pubkey|username`, `/api/wallets/:id/transactions[/sent|/received]` | wallets (`attributes.vote`, `attributes.delegate{...}`) |
| `GET /api/delegates`, `/api/delegates/:id`, `/api/delegates/:id/voters`, `/api/delegates/:id/blocks` | rank = vote weight (sum of voters' balances), produced blocks / forged fees from state |

`run` = optional snapshot import -> HTTP catch-up -> `--follow` loop (polls `/api/blockchain` every blocktime,
verifies and applies new blocks) with the API and a mempool pruner (drops forged / stale-nonce txs) running concurrently.
Sled indexes added for the API (`wp:`, `wu:`, `tl:`, `wt:`) - databases imported before this version must be re-imported.

## Legacy peer link (P2P port 4001)

`src/p2p_legacy/` speaks the inter-node protocol of the existing network: hapi-nes binary frames over
WebSocket + protobuf payloads (`p2p.peer.getStatus`, `p2p.peer.getPeers`, `p2p.blocks.getBlocks` - up to
400 blocks with transactions per call, no REST rate limit - and `p2p.transactions.postTransactions`).

```bash
sth-core peers                                                  # probe peers.json + seeds: latency, height, version
sth-core peer-status 138.199.164.235                            # handshake, height, peer list (use IPs!)
sth-core peer-status 138.199.164.235 --blocks 3 --from 11704042 # pull + verify blocks
sth-core --db-path ./data run                                   # P2P is the default intake (node.yaml p2p.legacy_enabled)
sth-core --db-path ./data run --peers 138.199.164.235,116.202.32.250 --parallel-peers 6
```

* **Peer health table** (`health.rs`): every peer keeps an EMA latency, reported height, consecutive failures and
  a temporary ban (3 failures -> 30 s, growing). `best(n)` = lowest `latency + 50 ms × blocks-behind + 1 s × failures`.
  The table is re-probed (`getStatus` + `getPeers` discovery) every `p2p.refresh_secs`; `/api/peers` serves it.
* **Parallel catch-up** (`follow.rs`): consecutive 400-block ranges are requested from the `parallel_peers` best peers
  concurrently and applied strictly in order; a failed range stops the round and is re-requested from another peer.
  Rounds are spaced ≥ 1.1 s (legacy nodes allow one `getBlocks` per second per IP and reset the socket otherwise).
  Sandbox: ~320 blocks/s with 4 peers, ~415 blocks/s with 6 (single peer ≈ 200 blocks/s).
* **Genesis**: legacy peers reset the connection on `getBlocks(lastBlockHeight = 0)`, so an empty database is
  seeded from the embedded, verified genesis block (`genesis/mainnet.json.gz`, 1855 transactions).
* **Transaction relay** (`relay.rs`): every transaction accepted by `POST /api/transactions` is sent to the
  `relay_fanout` best peers via `postTransactions` in parallel; REST `POST /api/transactions` on the bootstrap nodes
  remains the fallback when no peer accepted it.

Port 4001 must be addressed by **IP**: the `nodeN.smartholdem.io` hostnames are behind a reverse proxy that
returns 403 there. `run` loads https://github.com/smartholdem/data/blob/main/mainnet/peers.json (fallback: built-in `P2P_SEEDS`) and extends the table from `getPeers`.

Live follow polls the best peer every blocktime; if the node falls more than 800 blocks behind it switches back to
the parallel catch-up. Peers that reset the socket after a reply are reconnected transparently.

## Database size

Sled runs with zstd page compression (`use_compression`, factor 3): ≈ 0.7 KB/block on disk (≈ 8 GB for the
full chain, was ≈ 12 GB) at the price of a slower import (~15k blocks/s instead of ~28k).

## Web 4.0 layer (iroh)

```yaml
p2p:
  iroh:
    enabled: true
    secret_key_file: ./iroh.key      # created on first start; `sth-core iroh-id` prints the EndpointId
    bootstrap: [<EndpointId of another sth-core node>, ...]
    serve_blocks: true               # answer GetStatus / GetBlocks
    relay: true                      # n0 relays for NAT traversal (false for LAN-only)
```

Gossip topics `sth/<nethash>/blocks` and `sth/<nethash>/transactions` carry JSON (`GossipMessage`); a block at tip+1 is
verified and applied, a block further ahead triggers a `GetBlocks` gap-fill from the announcing peer. Transactions from
gossip enter the mempool and are relayed to legacy peers (and vice versa), so the two networks stay bridged while both
exist. `GET /api/node/peers` lists legacy and iroh peers with health data.

## Gateway mode: inbound legacy port

```yaml
p2p:
  legacy_listen: "0.0.0.0:4001"     # off by default
```

Old nodes connect to `ip:4001` only, so nodes without a static IP forge in push mode (they deliver `postBlock` /
`postTransactions` to the best legacy peers themselves). A few **gateway** sth-core nodes with a public IP enable
`legacy_listen`: they answer `getStatus` / `getPeers` / `getBlocks` / `postBlock` / `postTransactions` exactly like a
v3.8.2 node, so the legacy network sees them as ordinary peers and pulls blocks from them; between Rust nodes the
traffic goes over iroh. A netfory-provider endpoint with `local_ws_url: ws://127.0.0.1:4001` exposes the same port
to Web 4.0 clients, and `p2p.legacy_peers` accepts `ws://…` URLs for peers reached that way.

## Delegate module

```yaml
delegate:
  enabled: true
  secrets: ["delegate passphrase"]   # or secrets_file: ./delegates.json (legacy format { "secrets": [...] })
  broadcast_fanout: 6
```

Every 500 ms the forger derives the current slot (`timestamp / blocktime`), the round (`ceil(height / 21)`) and the
forging order (ranked top-21 shuffled with `sha256(round)`, exactly like the legacy core). When one of the configured
delegates owns the slot and the node is at the network tip, it assembles a block from the mempool (highest fee first,
nonce-consistent per sender, milestone limits), signs it, applies it locally and broadcasts it via legacy `postBlock`
and iroh gossip. Fees of the block go to the delegate wallet (block reward is 0 on mainnet today).

`GET /api/node/forging` shows the module state: delegates (rank / active), next own slot, last forged block and the
peers that accepted it, and the last skipped slots with reasons. Before forging the node asks the best 8 legacy peers
for their tip and only builds on a tip the majority agrees on (waiting for the previous slot's block if needed).

While following the tip the node keeps undo records for the last 1000 blocks; when a peer presents a block whose
`previousBlock` is not our tip (fork), the node rolls back (1, 3, 9, … blocks) and resyncs from the best peer.

Network parameters come from `network/mainnet/*.json` (embedded); `network_dir: ./network` in `node.yaml` plus
`sth-core init --network-files` let you edit milestones (e.g. re-enable forging rewards from block N) without rebuilding.

## Compatibility notes

* Block id = `sha256(header || DER signature)` (`idFullSha256` is on from height 1).
* Block signature = ECDSA/secp256k1, DER, low-S, over `sha256(header)`.
* Transaction id = `sha256(AIP-11 bytes)`; sender signature = legacy bip-schnorr (64 bytes)
  over `sha256(bytes without signature)`; ECDSA DER is auto-detected for non-64-byte sigs.
* Amounts / fees / nonces serialise as decimal strings (like core `BigNumber`), numbers accepted on input.
* `transform=false` API payloads from legacy nodes deserialise directly into `Block` / `Transaction`.

## Roadmap

- iroh: catch-up from iroh peers inside the download scheduler, `api://<EndpointId>/<provider>` client with Ed25519
  `pdata` verification (netfory-provider envelopes).
- Delegate module: live test on mainnet with a real active delegate; incremental vote-balance index.
- Cross-compilation / release packaging. See `CHANGELOG.md` for the history.
