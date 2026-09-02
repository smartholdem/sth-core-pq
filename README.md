# sth-core-rust - SmartHoldem Relay Node (Rust)

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
  sync.rs                Phase 3: batched legacy HTTP sync pipeline (reqwest + indicatif), resumable
  snapshot.rs            Phase 3.2: core-snapshots dump import (gzip records, msgpack), download latest .tgz
  crypto/tx_deserializer.rs, crypto/block_deserializer.rs  wire -> struct (inverse of the serialisers)
  api.rs                 Phase 4: axum REST API (legacy JSON), mempool.rs: tx pool + relay
  p2p_legacy.rs          legacy inter-node protocol client (nes/WebSocket + protobuf), run --p2p follow
  main.rs                CLI: run [--p2p] | peer-status | sync | snapshot | info | verify-block | import-block | wallet
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
> shortcut and will be replaced by iroh in Phase 5.

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

## Relay node: `run` (Phase 4 - local REST API + mempool + follow)

```bash
sth-core --db-path ./data run --from-dump latest --fast-import   # bootstrap, catch up, follow, serve API
sth-core --db-path ./data run                                    # resume + follow
CORE_API_HOST=127.0.0.1 CORE_API_PORT=4003 sth-core run          # legacy env names are honoured
```

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

`src/p2p_legacy.rs` speaks the inter-node protocol of the existing network: hapi-nes binary frames over
WebSocket + protobuf payloads (`p2p.peer.getStatus`, `p2p.peer.getPeers`, `p2p.blocks.getBlocks` - up to
400 blocks with transactions per call, no REST rate limit - and `p2p.transactions.postTransactions`).

```bash
sth-core peer-status node2.smartholdem.io                       # handshake, height, peer list
sth-core peer-status node2.smartholdem.io --blocks 3 --from 11704042   # pull + verify blocks
sth-core --db-path ./data run --p2p --nodes https://node2.smartholdem.io   # catch up + follow over P2P, API on 4003
```

`run --p2p` uses the peer link for both catch-up and live follow (every block is linked and verified before
`apply_blocks`); peers that reset the socket after a reply are reconnected transparently, unreachable hosts are
rotated. Note: not every public node exposes 4001 (node0/1/3 filtered from some networks; node2 answers).

## Database size

Sled runs with zstd page compression (`use_compression`, factor 3): ≈ 0.7 KB/block on disk (≈ 8 GB for the
full chain, was ≈ 12 GB) at the price of a slower import (~15k blocks/s instead of ~28k).

## Compatibility notes

* Block id = `sha256(header || DER signature)` (`idFullSha256` is on from height 1).
* Block signature = ECDSA/secp256k1, DER, low-S, over `sha256(header)`.
* Transaction id = `sha256(AIP-11 bytes)`; sender signature = legacy bip-schnorr (64 bytes)
  over `sha256(bytes without signature)`; ECDSA DER is auto-detected for non-64-byte sigs.
* Amounts / fees / nonces serialise as decimal strings (like core `BigNumber`), numbers accepted on input.
* `transform=false` API payloads from legacy nodes deserialise directly into `Block` / `Transaction`.

## Roadmap

3. ~~sync.rs~~ done (temporary bootstrap over REST API).
4. `api.rs` - axum on `127.0.0.1:4003` (`/api/blocks`, `/api/transactions`, `/api/wallets`, mempool).
5. `p2p.rs` - iroh endpoint, `sth_txs` gossip, `GetBlocks` RPC, `api://<NodeId>/<provider>` client
   with Ed25519 `pdata` verification (netfory-provider envelopes).
6. CLI `run` / cross-compilation.
