# Changelog

All notable changes to `sth-core` (SmartHoldem relay / delegate node in Rust). The version is bumped on every iteration.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow [SemVer](https://semver.org/).

## [0.9.3]

### Fixed
- **OOM on long-running nodes** (n0-computer/iroh#4509, still present in iroh 1.2.0): `pending_open_paths` in iroh's
  `RemoteStateActor` grew geometrically with several QUIC connections to the same remote (gossip + RPC) until the allocator
  failed - the node was killed by the kernel after hours on small VPS. iroh 1.1.0 is now vendored in `vendor/iroh`
  (`[patch.crates-io]`) with the retry queue deduplicated and bounded to 64 entries.

### Added
- `memory: rss N MB` INFO log every 10 min (Linux) and `node.rssBytes` in `/api/ntfry/metrics`; the metrics page shows RSS next to uptime.
- `db_cache_mb` in node.yaml (default 64): sled page cache size - set 16–32 on a 1 GB VPS.

## [0.9.2]

### Added
- Metrics page: **payout calculator** - cost of N payouts as SmartHoldem multipayments vs single transfers vs BNB Chain vs Solana
  (editable STH price in USDT, BNB/Solana per-transfer fee estimates); fees and `multiPaymentLimit` are read live from the node.
- `/api/ntfry/metrics` → `data.chain.multiPaymentLimit` and `data.chain.fees {transfer, multiPayment}` (smartoshi, current milestone).
- Docs (RU): `MULTIPAY-1024_RU.md` (blocktime × tx/block matrix for 1 024-recipient multipayments), `CONSENSUS_RU.md` §2a
  (finality explained, BFT-gadget protocol), `ARTICLE-BNB-SOLANA-STH_RU.md` (investor article with fee table at 1 STH = 0.0014 USD).

## [0.9.1]

### Added
- Every transaction entering the mempool (REST `POST /api/transactions`, legacy `postTransactions`, iroh gossip) is logged as one
  line: `Received transaction Transfer S… -> S… amount=1.50000000 STH fee=0.10000000 STH nonce=7 id=… memo="…"` (INFO);
  rejected ones as `Rejected transaction … : ERR_* reason` (WARN); full JSON at DEBUG (`RUST_LOG=sth_core::mempool=debug`).
  Type-specific summaries for Vote, DelegateRegistration, MultiPayment (total), Entity actions, HTLC.

### Fixed
- **Gateway not visible on other Rust nodes / `neighbors: 0`.** (1) The gossip neighbour flag was a single boolean shared by the
  two topics, so a `NeighborDown` on the transactions topic hid a live blocks neighbour - now tracked per topic. (2) A topic whose
  neighbours all dropped never re-bootstrapped: a watchdog re-joins the configured bootstrap peers and known RPC peers every 60 s
  while a topic has no neighbour. (3) The `Peers { gateway }` announcement is now broadcast on both topics (and handled on both),
  first 15 s after start instead of after 120 s. (4) `/api/ntfry/peers` and `/api/ntfry/metrics` report the node's **own**
  `gateway` address (`meta.gateway` / `data.ntfry.gateway`, `null` when not a gateway); the metrics page says
  "this node is a gateway (ip:port)". (5) `sth-core init` documents the gateway setup (`legacy_listen` + `legacy_public_addr`).
  Neighbour up/down events are logged at info level per topic.

## [0.9.0]

### Added - AIP-36 Entity transactions (consensus, activates at mainnet height 11 800 000)
- `network/mainnet/milestones.json`: `{ "height": 11800000, "aip36": true }` - same value the legacy `@smartholdem/crypto-networks`
  release must carry. Before that height entity transactions are rejected (`Entity transaction before aip36 activation`),
  legacy business/bridgechain types (group 2, types 0–5) are never accepted (none exist in the chain; legacy disables them at aip36).
- Wire format bit-compatible with `@smartholdem/core-magistrate-crypto` (`typeGroup 2`, `type 6`):
  `type u8 | subType u8 | action u8 | regIdLen u8 | registrationId | nameLen u8 | name | ipfsLen u8 | ipfsData`
  (`crypto/tx_serializer.rs`, `tx_deserializer.rs`; JSON asset `{ type, subType, action, registrationId?, data: { name?, ipfsData? } }`).
- Rules exactly as `EntityTransactionHandler` (`rules.rs::check_entity_format / check_entity`): amount 0, exact static fee
  (register 50 STH, update/resign 5 STH - verified identical in the SmartHoldem fork), name `^[a-zA-Z0-9_!@$&.-]{1,40}$`,
  ipfsData base58 ≤ 128, network-wide unique `(name, type)` case-insensitive, Delegate entity requires the sender's username,
  update/resign only by the owner with matching type/subType, no update/resign after resign. Legacy error names are kept.
- State: `WalletState.entities[<registrationId>] = { type, subType, data, resigned }` (`attributes.entities` in `/api/wallets`),
  `en:<name>-<type>` index for uniqueness/search; rollback restores wallets and drops the index of undone registrations.
- Mempool: activation gate, rules, one pending registration per `(name, type)` (`ERR_PENDING`).
- API: `GET /api/entities` (filters type/subType/name/isResigned/address/publicKey/id), `GET /api/entities/{id}`,
  `POST /api/entities/search`; `/api/transactions/types|fees` switch group 2 to `Entity` after activation;
  `constants.aip36` in `/api/node/configuration`.
- Tests `tests/entity.rs`: wire layout, activation gate, lifecycle/errors/state/rollback, batch register+update, mempool.
- Startup prints `core version X.Y.Z` (bold blue on a terminal, plain when piped to pm2 / a file).

## [0.8.3]

### Added
- **Second-signature enforcement** exactly like legacy `TransactionHandler.throwIfCannotBeApplied` (`src/rules.rs`):
  a wallet with a registered second public key must second-sign every transaction (`MissingSecondSignatureError` /
  `InvalidSecondSignatureError`), a wallet without one must not (`UnexpectedSecondSignatureError`), a second registration
  is rejected (`SecondSignatureAlreadyRegisteredError`), and the registration key must be a 33-byte compressed secp256k1 key.
  Applied wallet-aware and in order inside `sync::apply_blocks` (registrations earlier in the same batch count) and in the
  mempool (a registration still pending in the pool already binds the sender). Until now the node only stored
  `secondPublicKey` and never checked it. Mainnet history has a single registration (height 8 948 990, no spend after it),
  so re-verification of the chain is unaffected. Tests: `tests/second_signature.rs`.

### Performance
- **Signature verification 8× faster** (`tests/bench_block.rs`, 4 vCPU): block of 500 transfers verify 411 → 52 ms,
  10 000 transfers 8.2 s → 1.0 s (verify + apply 9.1 s → 1.95 s - a 10k-tx block now fits an 8 s slot).
  - `crypto/schnorr.rs`: the quadratic-residue test used `num-bigint` modpow on every signature (≈ 0.5 ms); now k256's
    native `FieldElement::sqrt()`. `s·G − e·A` is one Shamir/Straus `lincomb_ext` instead of two scalar multiplications.
    Single-thread cost 0.82 → 0.24 ms per signature. `num-bigint` dependency removed (`k256` feature `expose-field`).
  - `verify_block`: per-transaction id + signature checks run on all cores (`rayon`); `apply_blocks` verifies a whole
    catch-up batch (≤ 400 blocks) in parallel before the sequential link check and the sled write.
  - Catch-up with `sync.verify_blocks: true` is bound by peers, not CPU, at this point.


### Added
- **Operator metrics page** (`api.page_metrics: true`, default off): terminal-style dashboard - height / network
  height / last-block age / uptime, legacy + ntfry peer counts, mempool, forging with next-slot countdown, last blocks,
  peer tables. Served at `http://<api host>:<port>/` or on its own address with `api.metrics_listen: "0.0.0.0:4888"`
  (that listener exposes only the page and `/api/ntfry/*`, nothing else from the API). `/` keeps `Hello World!` when off.
- `GET /api/ntfry/peers` - Web 4.0 (iroh) peers by EndpointId only (never IPs): height, latency, neighbor/known,
  gateway flag; `meta.nodeId` is our own EndpointId. `GET /api/ntfry/metrics` - one JSON snapshot for dashboards.
- **Block intake stats** (`intake.rs`, shown on the page as *block source* / *slot delay*): which channel delivered the
  last live block - `pull/legacy` (follow loop), `push/legacy` (inbound `postBlock`), `gossip/iroh`, `forged` - from
  which peer, and the delay between the block's slot start and its arrival (last, average of the last 100, per source).
  Exposed as `data.intake` in `/api/ntfry/metrics`.
- `tests/bench_block.rs` (ignored): block throughput probe - sign / forge / serialize / verify / apply N transfers
  (`BLOCK_TXS=10000 cargo test --test bench_block -- --ignored --nocapture`). Dev profile now compiles k256 / sled with
  `opt-level = 3` so tests and probes measure real crypto speed.

### Changed
- `rewards.reward_passphrase` removed (it only derived an address that nothing uses; no reason to keep a second secret
  in the file - old configs with the key still load). `rewards.reward_address` stays as a reserved field for future
  relay/gateway rewards; comments now say so explicitly and that block rewards always go to the forging delegate's
  wallet. The "no reward address configured" startup line is gone.

### Fixed
- **Catch-up stalled for minutes** (bursts of 400 blocks, then `0 blocks/sec`): block sources were ranked by
  `getStatus` latency (tens of ms) while `getBlocks` for 400 blocks takes 20 s+ on many legacy Node.js peers and hit the
  socket timeout; every failed attempt cost a worker 20 s, and the 60 s health refresh (`record_success` after a fast
  `getStatus`) reset the failure counter / ban, so the same slow peers were picked again.
  `PeerStats` now keeps a separate `getBlocks` metric - measured speed (EMA per full range), consecutive failures and
  a parking timer (30 s → 60 s → … → 10 min) that status probes do not lift. `best_for_blocks()` ranks by that metric,
  skips parked peers, and the workers rather wait 150 ms for a fast idle peer than burn a timeout on a slow one.
- First `getBlocks` to a peer of unknown speed asks for 100 blocks (probe); subsequent requests are sized so the reply
  fits a 12 s budget (`blocks_limit()`, 50…400). Ranges are scheduled as `(from, to)`, so short replies re-queue only
  their tail - no gaps, no overlaps.
- `GET /api/peers` legacy entries show `blocksLatency`, `blocksLimit`, `blocksFailures`, `blocksParkedFor`.
- **Legacy peers reset the next connection from the same IP for ~1 s after a `getBlocks` reply** (`Connection reset
  without closing handshake`): the per-peer spacing is now measured from the previous reply (1.5 s) instead of the
  request start. Measured on mainnet from genesis (debug build): 0 resets, ~1 150 blocks/s vs ~150 blocks/s and
  50 resets/100 s before. The live follow loop treats such a reset right after connecting as expected (no WARN, no penalty).


### Fixed
- **Forging order was wrong** (`delegate/round.rs::shuffle`): the legacy `shuffleDelegates` loop is
  `for (i…; i++) { for (x < 4 && i < n; i++, x++) … }`, i.e. the outer `i++` skips indices 4, 9, 14, 19 after every
  batch of four swaps. Our port swapped every index, so the node predicted the wrong slot and never forged
  (the network showed an empty slot for `nicholasflamel`). Verified against mainnet round 557584: 21/21 generators
  now match; regression test `shuffle_matches_mainnet_round_557584`.
- `delegate.enabled: true` without a passphrase no longer aborts the node with `config error: … no secrets configured`:
  the problem is reported as an ERROR with a ready-to-paste `delegate:` snippet and the node continues as a relay.
  Secrets are resolved before peer discovery, so the message appears immediately.
- `peers.json` download: the full error cause chain is logged (DNS / timeout / TLS instead of just
  `error sending request`), one retry after 2 s, and the fallback line states how many built-in seeds are used.
- Live follow: the socket reset legacy nodes perform right after a `getBlocks` reply is no longer logged as
  `WARN getStatus failed, switching peer` and no longer penalises the peer; `following chain via legacy peer` is logged
  only when the peer actually changes.

### Changed
- Delegate identity is printed at start-up from the chain state, one line per key:
  `Delegate nicholasflamel: address S…, rank 12 of 21 (active), public key 02c9…`. When the database is empty or still
  syncing, the forger re-resolves username / rank every 30 s until the delegate is found.

## [0.8.0]

### Added
- `GET /status` - self-contained HTML status page on the local API (height, sync state, legacy + iroh peers with
  gateways, delegate slots / forging history, last blocks; refreshes every 5 s, no external assets).
- Gateway autodiscovery: `p2p.legacy_public_addr` ("ip:4001") is announced in the iroh `Peers` gossip message
  (`gateway` field, every 2 min); receivers add the gateway to their legacy peer table and list it in
  `/api/node/peers` meta `gateways` and per-peer `gateway`.
- `docs/TRANSITION_RU.md` - Russian operator guide: Rust delegates on dynamic IPs + legacy delegates on static IPs.

## [0.7.0]

### Added
- Forger quorum now includes iroh peers (`GetStatus` over `sth/rpc/1`) and the required share of agreeing peers is
  configurable: `delegate.quorum_share` (0.0–1.0, default 0.5, validated).
- NTP clock check at start (`src/ntp.rs`, SNTP to pool.ntp.org): `Your NTP connectivity has been verified … Local clock
  is off by Nms`, WARN when the drift exceeds 1 s (slots are 8 s wide). Unit-tested against a mock SNTP server.
- Peer reputation sharing: every 5 minutes a node publishes its healthiest legacy peers (`GossipMessage::Peers`) on the
  iroh transactions topic; receivers add unknown IPs to their peer table with the reported height / latency / version,
  so freshly started Rust nodes skip the probing phase.

## [0.6.0]

### Added
- **Inbound legacy P2P server** (`p2p_legacy/server.rs`, `p2p.legacy_listen: "0.0.0.0:4001"`): nes/WebSocket handshake,
  ping, `p2p.peer.getStatus`, `p2p.peer.getPeers`, `p2p.blocks.getBlocks` (same `[u32 BE len][BlockHeader]*` codec as
  the legacy nodes), `p2p.blocks.postBlock` (chained → verified → applied, re-posts acknowledged, orphans rejected),
  `p2p.transactions.postTransactions` (→ mempool → relay/gossip). Gateway nodes with a public IP become regular peers
  for old nodes; the same port can be published through netfory-provider (`local_ws_url: ws://127.0.0.1:4001`).
- `LegacyPeer::connect` accepts full `ws://` / `wss://` URLs, so `p2p.legacy_peers` may point at Rust nodes reached
  through a Web 4.0 proxy instead of `ip:4001`.
- Console numbers grouped like the legacy core (`Received new block at height 11,708,910 …`, `Broadcasting block 11,708,910 …`).
- `GET /api/node/forging` - delegate module state: configured delegates (username, rank, active), next own slot,
  last forged block (height, id, tx count, peers that accepted it), last 20 skipped slots with reasons.

## [0.5.0]

### Added
- **Iroh catch-up**: the download scheduler pulls 400-block ranges from iroh peers (fastest peer that has the range
  first, no per-second limit) alongside legacy IPs; `IrohNode::refresh_peers()` probes heights on start and every
  `p2p.refresh_secs`; `IrohNode::add_peer_addr()` / `MemoryLookup` for pinned LAN peers. Integration test: node B syncs
  two blocks forged by node A over iroh only.
- **Fast vote index** (`dv:<publicKey>` → vote weight): updated incrementally on every wallet write and on rollback;
  `delegate_ranking()` is now O(delegates) instead of scanning all wallets; existing databases are migrated once at
  start (`ensure_vote_index`). `rebuild_vote_index()` for manual repair.
- Legacy-style console output: `Loaded N active delegate(s): name (publicKey)` (with a warning when the key is not
  registered or outside the top-21), `Next forging delegate … is active on this node.`, `Forged new block … by delegate …`,
  `Broadcasting block H to N peers`, `Received new block at height H with T transactions from IP`,
  `Downloaded N new blocks …`, `Blockchain 100% in sync`, `Starting Round R`.
- `GET /` → `{ "data": "Hello World!" }` (legacy root probe used by netfory-provider) and legacy JSON 404 envelope for
  unknown routes.

- Forger quorum check: before forging, the best 8 legacy peers are asked for their tip; the node waits (until 2 s before
  the slot ends) for the previous slot's block to arrive and forges only when the majority reports our exact tip
  (height + id). Every owned slot is logged with the outcome (`Slot N belongs to delegate …`, `Skipping slot N …: reason`).

### Changed
- `P2pOptions` carries the optional iroh handle; block sources are `Source::Legacy(ip)` / `Source::Iroh(id)`.
- Live follow polls the best peer every 1.2 s instead of once per blocktime so new blocks are seen within ~1 s.

## [0.4.0]

### Added
- **Legacy API compatibility**: `GET /api/transactions/types`, `/api/transactions/schemas`, `/api/node/configuration/crypto`
  (network.json / milestones.json / exceptions.json / genesisBlock.json), `/api/peers/:ip`, `/api/votes`, `/api/votes/:id`,
  `/api/locks`, `/api/locks/:id`, `POST /api/locks/unlocked`, `/api/wallets/top`, `/api/wallets/:id/votes`,
  `/api/wallets/:id/locks`, `/api/entities`, `/api/entities/:id`, `/api/rounds/:round/delegates`.
- Full legacy filter set: transactions (`senderPublicKey`, `vendorField`, `version`, `sequence`, `timestamp/amount/fee/nonce`
  ranges), blocks (`generatorPublicKey`, `timestamp` ranges), wallets (`address`, `publicKey`, `balance`/`nonce` ranges,
  `orderBy`), delegates (`username`, `address`, `publicKey`, `isResigned`, numeric ranges, `orderBy`).
- `GET /api/node/peers` - sth-core extension: live health table of legacy and iroh peers (latency history, height lag,
  score, failures, ban state, source).
- **Iroh layer** (`p2p_iroh/`): endpoint + `Router`, gossip topics `sth/<nethash>/blocks` and `sth/<nethash>/transactions`,
  JSON RPC on ALPN `sth/rpc/1` (`GetStatus`, `GetBlocks`), gap-fill from the announcing peer, mempool bridge
  (gossip → mempool → legacy `postTransactions` and back). `node.yaml` section `p2p.iroh` (`enabled`, `secret_key_file`,
  `bootstrap`, `serve_blocks`, `relay`), CLI `sth-core iroh-id`.
- **Delegate module** (`delegate/`): round/slot arithmetic and the legacy deterministic delegate shuffle, block assembly
  (fee-ordered, nonce-consistent, milestone limits), ECDSA signing, local apply, `postBlock` broadcast to legacy peers
  (+ iroh gossip). `node.yaml` section `delegate` (`enabled`, `secrets`, `secrets_file` = legacy `delegates.json`,
  `broadcast_fanout`). Round snapshots (`rd:<round>`) recorded at every round boundary.
- **Rollback**: undo records per block (`undo:<height>`, depth 1000) while following the chain; `rollback_last_block` /
  `rollback_to`; fork detection in the follow loop rolls back with growing depth and resyncs from the best peer.
- **Network files** in `crypto-networks` layout embedded (`network/mainnet/*.json`), `network_dir` in `node.yaml` and
  `sth-core init --network-files` to export/edit them (milestone-driven parameters, e.g. re-enabling rewards from block N).
- State for `DelegateResignation` (`resigned`), `HtlcLock` (`attributes.htlc.locks` / `lockedBalance`, `lk:` index),
  `MultiSignature` (combined-key address, `attributes.multiSignature`); Ipfs intentionally not tracked.
- `legacy_peer.post_block`, `serialize_block_with_transactions`, `PostBlockRequest/Response` protobuf messages.

### Changed
- `api.rs` split into `api/{mod,render,node,blocks,transactions,wallets,delegates,locks}.rs`.
- Catch-up is now pipelined: every worker fetches its next 400-block range as soon as it is free (shared scheduler with
  retry queue and per-peer 1.1 s spacing) instead of lock-step rounds - ~400 blocks/s with 4 peers in the sandbox.
- `Network` is data-driven (milestones deep-merged like the legacy `configManager`); `Network::mainnet()` is a cheap
  clone of a cached instance (`mainnet_ref()`).
- `/api/node/fees` computes real avg/min/max/sum per type over the last `days`.
- `/api/node/status` and `/api/node/syncing` use the peer table's best height.

### Fixed
- Verifying a block re-parsed the embedded genesis for every transaction (`Network::mainnet()` in the serializer) -
  genesis verification took > 60 s; now ~4 s in debug.

## [0.3.0]

### Added
- `node.yaml` configuration (`sth-core init`, `run --config`; CLI flags override the file), sections
  `api / sync / p2p / rewards / mempool`; `rewards.reward_address` or `reward_passphrase` (address derived, logged at start).
- `NodeContext` (`node.rs`) assembling storage, mempool, peer table, API and block intake from the configuration.
- Legacy P2P split into `p2p_legacy/{proto,client,health,follow,relay}.rs`: peer health table (EMA latency, height,
  consecutive failures, bans), parallel catch-up from the best peers, live follow, transaction relay via
  `postTransactions`; `sth-core peers` command; `/api/peers` served from the health table.
- Embedded, verified genesis block (legacy peers reset the socket on `getBlocks(0)`).
- `orderBy=…:asc` tests for blocks / transactions / wallets.

## [0.2.0]

### Added
- Legacy inter-node protocol client (nes framing over WebSocket + protobuf): `getStatus`, `getPeers`, `getBlocks`,
  `postTransactions`; `run --p2p`, `peer-status`; peers.json auto-discovery from GitHub.
- Sled zstd compression and 64 MiB cache (~0.7 KB per block).
- Local REST API (axum, port 4003) mirroring the legacy JSON, pagination `meta` links byte-for-byte, delegate ranking.
- Snapshot importer (`.tgz` dumps, `--fast-import`), `--from-dump latest` download.

## [0.1.0]

### Added
- Cryptographic primitives (secp256k1 ECDSA / Schnorr, SHA-256, RIPEMD-160, base58check addresses), transaction and
  block serializers / deserializers with legacy test vectors.
- Sled storage (blocks, transactions, wallets, delegates), rate-limit aware HTTP sync from legacy REST nodes, CLI.
