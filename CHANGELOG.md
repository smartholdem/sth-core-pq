# Changelog

## [0.19.0]

### Added - Quantum Shield stage B (v3 transactions, ML-DSA-44) behind milestone `pq`
- Wire format v3: type-1 payload `alg || pk_len || pk`, second-signature section as `alg || len || sig` blocks,
  JSON `secondSignatures: [{ algorithm, signature }]`, `asset.signature.algorithm`; `M2 = sha256(BODY || SIG1)`, ML-DSA context
  `"sth-pq-v1"` (spec aligned with the code and the published vectors).
- Milestone `pq: { active, feePerByte (10 000), commitmentGrace (86 400) }`; `Network::pq_activation_height()`; `sth-core init newnet --pq-at H`.
- Rules (`rules::check_pq_format` / `check_pq`): activation gate, algorithms / sizes / order, fee surcharge (core ≥ static + surcharge,
  sObject / token exact + surcharge), registration with proof of the current key (legacy alg 0 or old PQ key), rotation, commitment
  grace window, PQ-locked wallets must send v3 with exactly one block; batch-aware in `check_stateful_rules`, pending-registration-aware
  in the mempool (`ERR_PQ_*` codes). `WalletState.pq_key { algorithm, publicKey, since }` replaces `secondPublicKey` on registration.
- API: `configuration.pq`, `quantumShield.stage`, wallets `quantumShield.{active, algorithm, publicKey, since}`, transactions `secondSignatures`.
- CLI: `sth-cli tx pq-register [--old-second-passphrase]`, `--second-passphrase` / `STH_SECOND_PASSPHRASE` for every `tx ...` (legacy v2
  second signature or automatic v3 + surcharge for PQ-locked wallets); `sth-cli wallet` shows the shield state.
- Tests `tests/pq_v3.rs`: wire round-trip, activation, registration / lock / rotation / rollback, legacy migration + commitment window, mempool codes.

### Added - operations
- Mempool **byte budget** `mempool.max_bytes` (default 8 MB, `Mempool::with_max_bytes`): `ERR_POOL_FULL` once the wire bytes of pending
  transactions would exceed it; `/api/ntfry/metrics` -> `mempool.bytes / maxBytes`, `/api/node/configuration` -> `maxBytesInPool`.
- Metrics page: `quantum shield stage A|B: N committed · M PQ-active · pool KB` (`pqk:` index, `pq_key_count()`, `node.pqStage/pqActive`).
- `tests/vectors/pq_v3.json`: `v3` format notes + `transactions` (5 byte-exact v3 vectors: registration fresh / with legacy proof,
  PQ-locked transfer, rotation, v3 with alg-0 block) verified by `published_v3_transaction_vectors_match`.
- design draft (hybrid secp256k1 + ML-DSA-44 block signatures, delegate PQ key, `pq.blocks` milestone, verification order).

- `sth-cli pq-commitment` (stage A helper: prints the `sthpq1:` commitment + ready `tx transfer --memo` line)

### Added - sObject safety
- **Resign guard**: `obj-resign` of a type-5 registry whose token has live supply is refused (`TokenSmartObjectStillActiveError`) in blocks
  and in the pool; burn the supply to 0 first.
- **Pool spend fix**: the mempool balance check counts the price of a pending sObject buy for later transactions of the same sender.

## [0.18.0]

### SmartObject (sObject)
- The programmable on-chain object of `typeGroup 2 / type 6` is now a **SmartObject (sObject)** everywhere the operator can
  see it: Rust types (`SmartObject`, `SmartObjectAsset`, `SmartObjectData`, `models::sobj`), storage API (`get_sobject`,
  `all_sobjects`, `sobj_by_name`), rules (`check_sobj*`), error names (`SmartObjectNotRegisteredError`,
  `SmartObjectNotForSaleError`, `TokenNotSmartObjectOwnerError`, ...), log lines and the metrics page.
- REST: the old legacy-named routes **removed**, replaced by `GET /api/sobj`, `GET /api/sobj/:id`, `POST /api/sobj/search`
  (same JSON shape). Wallet attribute renamed to `attributes.sobjects`; `/api/tokens` field renamed to `sobj`;
  `/api/transactions/fees` group 2 keys `sobjRegistration / sobjUpdate / sobjResignation`; `/api/node/configuration` ->
  `constants.sobjV2`, `tokenTickerSobjType`; transaction type name of group 2 is `SmartObject`.
- Milestone flag renamed to **`sobjV2`** (the legacy-named flag is still read as an alias). `sth-core init newnet` writes `sobjV2: true`.
- CLI: `sth-cli obj <id|name> [--type N]`, `sth-cli tx obj-register / obj-update / obj-transfer / obj-sell / obj-buy`
  (same for `sth-core tx`). `watch` prints "sObject registered / handed over / bought".
- **Wire format is untouched**: typeGroup 2 / type 6 bytes, `asset.{type,subType,action,registrationId,data}` JSON, transaction ids
  and block ids are byte-for-byte the same as before; Sled prefixes `en:` / `eo:` / `mk:` are kept (commented as sObject keys), so
  existing databases need no migration.
- TokenInit fee default **100 -> 500 STH** (`tokenFees.init = 50000000000`), half of it still burned (`INIT_BURN_SHARE_PERCENT = 50`).
  Test networks created with older `milestones.json` keep their own value.

### Added
- Milestone parameter **`tokenFees.initBurnPercent`** (0–100, default 50) replaces the hard-coded 50 % TokenInit burn share:
  `{ "height": H, "tokenFees": { "initBurnPercent": 0 } }` switches the burn off (or changes the share) without a code fork.
  Every `tokenFees` field now has a default so a later milestone may patch a single key; values > 100 are rejected at load.
  `/api/node/configuration` exposes `tokenFees.initBurnPercent` and `initBurn` (smartoshi per TokenInit); `sth-cli status`
  prints `token init 500 STH · burn 50% (250 STH)`. Test: two milestones (50% -> 0%) in one chain, rollback across the switch.
- full specification of SmartObjects: types, lifecycle (register / update / resign / transfer), market (sell / buy), `ntfryData`, fees and burn, API, CLI, JSON examples, limits, error catalogue.

## [0.17.0]

### Added
- `sth-cli create-wallet [--passphrase]`: BIP-39 12-word mnemonic from the OS random generator (`crypto::mnemonic`, official English
  wordlist embedded, checksum verified against the BIP-39 vectors), address with the connected network's byte, public key. Offline -
  nothing is sent to the node.
- `sth-cli watch <address> [--every N] [--tail N]`: live wallet feed - coins in / out with memo, multipayments, votes, HTLC, token
  transfers / mint / burn / manifest, SmartObject registration / order / sale / purchase / hand-over; `pool` vs `✓confirmations`,
  `--json` streams one transaction per line.
- Wallet history now includes token recipients (transfer items, mint) and the new owner of a transferred / bought SmartObject
  (`tx_addresses` index + `/api/transactions?recipientId=` filter) - previously only STH recipients were indexed.

## [0.16.1]

### Added
- Release pipeline: `.github/workflows/release.yml` builds `sth-core` + `sth-cli` on tag `v*` for linux x86_64 / aarch64 and macOS
  arm64 / x86_64, smoke-tests `--version`, packs tar.gz + `SHA256SUMS` and attaches them to the GitHub release;
  `.github/workflows/ci.yml` builds and runs the test suites on every push (fmt / clippy advisory). `scripts/release.sh` produces the
  same archive locally.

## [0.16.0]

### Added - `sth-cli`, a node client binary (bitcoin-cli style)
- Second binary in the crate, HTTP only (no Sled / P2P): `status`, `wallet`, `token`, `SmartObject`, `market`, `tx-status --wait`, and
  every signed transaction as `sth-cli tx ...` (transfer, obj-register / update / transfer / sell / buy, token-init / transfer / mint /
  burn / meta). Chain parameters (address byte, fees, blocktime) come from `GET /api/node/configuration`, so it works against any
  node - mainnet or a private network - with `--api URL` (remembered in `~/.config/sth-cli/api`) or `STH_API`. `--json` everywhere.
- Signer moved into the library (`sth_core::cli`, `ChainSource::{Config, Api}`); `sth-core tx ...` keeps working on the node itself.
- Nonce race handled: when earlier transactions of the sender are still in the pool the node's `expected N` is used and the
  transaction is re-signed and re-sent once. `tx-status --wait` waits until the transaction leaves the pool.

## [0.15.0]

### Added
- **Market tab** on the metrics page (node | tokens | market): every open sale order - SmartObject name, type badge, price, owner,
  pointer, id, token logo / supply for tickers - 100 per page with prev / next. Feed `GET /api/ntfry/market?page=&limit=&type=`
  (`meta.totalCount / pageCount`). Backed by a new Sled index `mk:<id>` -> owner, maintained on sell / cancel / buy / transfer /
  resign and restored on rollback, so listing thousands of orders is a prefix scan, not a wallet sweep.
- **Version guard** on the Delegate Dashboard: milestone `minCoreVersion` (e.g. "0.14.0"; `init newnet` writes the generator's
  version). Rust delegates announcing an older version are marked `⚠ update to ≥ X` in red, the bar shows `N below vX`;
  `/api/ntfry/delegates` returns `outdated` per delegate plus `outdated` / `minCoreVersion` totals. Exposed in
  `/api/node/configuration` constants together with `strictBalance`.

## [0.14.0]

### Added - sender balance enforced in block validation (milestone `strictBalance`)
- `check_stateful_rules` now tracks every STH movement inside the validated batch - sender debit `amount + fee (+ multipayment
  sum)`, transfer / multipayment credits, HTLC claim / refund credits (locks opened in the batch included), SmartObject purchase price,
  forger reward + fees **after** the block's transactions (legacy order, minus the TokenInit burn) - and rejects a block whose sender
  cannot cover the spend: `InsufficientBalanceError: sender ... has X, needs Y`, exactly like legacy `throwIfCannotBeApplied`.
  Coins received earlier in the same block count; coins arriving later or the forger's own reward of that block do not. Height 1 is
  exempt (genesis distributes from nothing).
- Milestone flag `strictBalance` (default **false**, `init newnet` -> true). With the flag off the node applies the block and logs
  `WARN balance check would fail (milestone strictBalance is off)` - mainnet operators can resync history and confirm there are no
  hits before the flag is switched on in the same milestone as `tokens` / `sobjV2`.
- Mempool: an SmartObject purchase is checked against the balance **minus** the sender's pending pool spend.
- Tests `tests/balance.rs`.

## [0.13.1]

### Fixed
- **Catch-up stalled on a block with an SmartObject purchase** (`SmartObjectInsufficientBalanceError` -> "peer blocks rejected, re-requesting
  range" forever): batch validation checked the buyer's balance against the pre-batch wallet, ignoring coins received earlier in the
  same 400-block range. `check_stateful_rules` now tracks STH moved inside the batch (transfers, multipayments, purchase prices) and
  validates the purchase against the effective balance. Blocks applied one by one (live follow) were never affected.
- A private network's first node now learns about nodes that dial in: an inbound legacy connection that speaks `p2p.blocks.*` is
  added to the peer table (verified by the next status probe), so forged blocks are pushed to it (`Broadcasting block ... to 1 peers`)
  instead of `0 peers`.
- Catch-up diagnostics: with ≤ 3 known peers every failed `getBlocks` is logged at WARN with the reason (timeout, empty reply, ...);
  on a tiny network a failing block source is parked for at most 5 s instead of 30 s -> 10 min.

## [0.13.0]

### Added - SmartObject market (milestone `sobjV2`)
- **Sell order** (`action 4`, fee 1 STH): the owner sets `asset.price` (smartoshi) on any non-delegate SmartObject; `price 0` cancels.
  The order is stored in the SmartObject record (`price`, `forSale` in `/api/sobj*` and `/api/tokens*.SmartObject`).
- **Buy** (`action 5`, fee 1 STH): anyone pays the recorded price in the same transaction - coins go to the current owner, the SmartObject
  (and, for a ticker, the token registry: owner, supply, manifest, mint / meta rights) goes to the buyer, the order closes. Atomic:
  no escrow, no trust. Checks: open order, buyer ≠ owner, buyer balance ≥ price + fee (`SmartObjectNotForSaleError`,
  `SmartObjectTransferToSelfError`, `SmartObjectInsufficientBalanceError`). Sell + buy + re-list inside one block resolve in order; rollback
  restores owner, order and balances (the previous owner comes from the undo snapshot, not from the transaction sender).
- Wire: `price` travels in the `ntfryData` slot as a decimal string - SmartObject byte layout unchanged. `sth-core tx obj-sell --id X
  --price 2500`, `sth-core tx obj-buy --id X`. Tests `tests/sobj.rs::sobj_sell_and_buy`.
- `rules::check_sobj` takes a batch-aware `lookup(registrationId)` closure (record lives in the seller's wallet, not the buyer's).

## [0.12.0]

### Added
- **SmartObject transfer** (`typeGroup 2 / type 6`, `action 3`, milestone `sobjV2`): the owner hands an SmartObject - and, for a type-5 ticker,
  the whole token registry (owner, supply, manifest, mint / meta rights) - to `asset.recipientId`. Fee 5 STH. Token balances stay where
  they are. Refused for delegate SmartObjects and self-transfers; the old owner loses update / mint / meta rights immediately (also inside
  the same block: A->B->C in one block ends with C). On the wire the recipient travels in the `ntfryData` slot, so the byte layout of
  SmartObject transactions is unchanged. New index `eo:<id>` -> current owner (absent = registrant); rollback restores the previous owner
  and the `tk:` token index. `sth-core tx obj-transfer --id COFFEE --to D...`. Tests in `tests/sobj.rs`.
- Metrics page: **tokens tab** (node | tokens) - token count / manifests / issue fee / activation status cards, token explorer table
  (logo from the chain, name + website, supply / cap bar, decimals, flags, owner, issue height) and the latest issues.
  Feed `GET /api/ntfry/tokens`; the dedicated metrics port also serves `/api/tokens/{id}/logo`.
- `constants.sobjV2`, `constants.tokenMetaMaxLogoBytes`, `tokenFees.meta` in `/api/node/configuration`; `/api/transactions/types`
  lists group 3 once tokens are active.

## [0.11.0]

### Added - TokenMeta: the token manifest lives in the chain (no IPFS, no external storage)
- New typeGroup-3 type 4 **TokenMeta**: `asset.token.meta { name ≤ 64, description ≤ 512, website ≤ 128, logoType svg|png, logo base64 }`,
  logo file **≤ 8192 bytes** (PNG signature checked; SVG must be UTF-8 `<svg`/`<?xml`, no `<script`, `javascript:`, `<foreignObject>`).
  Owner only, token must be initialized, fee `tokenFees.meta` (default 1 STH), last one wins, rollback restores the previous manifest.
  Wire: `nameLen u8 | name | descLen u16 | desc | siteLen u8 | site | logoType u8 | logoLen u16 | logo`. Stored in `TokenState.meta`
  (owner wallet registry -> existing undo mechanism). Dormant on mainnet like the rest of typeGroup 3.
- API: `meta { name, description, website, logoType, logoSize, logoUrl }` in `/api/tokens*` and `attributes.tokensIssued`;
  `GET /api/tokens/{id}/logo` serves the image (`image/svg+xml` / `image/png`, cache + CSP headers). Logo bytes never appear in JSON.
- `sth-core tx ...` CLI signer for private networks / tests: `transfer`, `obj-register`, `obj-update`, `token-init`,
  `token-transfer` (`--to ADDR:AMOUNT` ×N), `token-mint`, `token-burn`, `token-meta --logo file.svg|png`. Human amounts with
  decimals, nonce / fees / ids resolved through the local API, `--dry-run` prints the signed JSON, passphrase via `STH_PASSPHRASE`.

### Changed - SmartObjects
- JSON field `ipfsData` -> **`ntfryData`** everywhere (models, API, docs); the old key is still accepted on input. Wire bytes unchanged
  (the name never went on the wire; legacy peers receive serialized blocks). Mainnet has no SmartObject transactions yet, so no migration.
- Milestone **`sobjV2`** (default false; `init newnet` sets true): `ntfryData` = any UTF-8 text 1–255 bytes without control
  characters (legacy: base58 ≤ 128), and a type-5 (ticker) registration must match `^[A-Z0-9]{3,10}$` (`SmartObjectTickerInvalidError`) -
  the network enforces it instead of relying on wallet-side filters. Legacy rules stay in force on mainnet until the flag is set.

### Fixed
- Mempool rejected every TokenInit with `Transfer without recipient` (type 0 check ignored `typeGroup`).
- `/api/tokens/{key}`, `/holders`, `/api/wallets/{address}/tokens` returned 404: routes used `{key}` syntax instead of axum 0.7 `:key`.

## [0.10.1]

### Added - `sth-core init newnet`: private / test networks in one command
- Generator (`src/newnet.rs`): `network.json` (title, ticker, `pubKeyHash`, nethash = genesis payload hash, burn address),
  `milestones.json` (mainnet rules collapsed into one milestone at height 1, `activeDelegates = N`, `tokens: false`, optional
  `--tokens-at H`), empty `exceptions.json`, a signed `genesisBlock.json` (treasury premine, stake + registration + vote for each
  delegate), `delegates.json` (passphrases, tests only) and a ready `node.yaml` (all N delegates forging, `network_dir: .`,
  separate ports 4002 / 4004 / 4889, `rest_nodes: []`, `legacy_peers: []`, iroh off). `--seed` makes the network deterministic.
- The node applies the genesis block from `network_dir` into an empty database before forging starts (no snapshot / REST needed).
- `delegate.quorum_share: 0.0` now explicitly means "private network": the forger takes its slot even when no peer answers
  `getStatus` (mainnet default 0.5 / `min_quorum_peers 3` unchanged - an isolated mainnet node still skips its slot).
- Legacy P2P seed peers are only added for the network named `mainnet`; a private network never contacts mainnet nodes.
- `--config` is a global CLI flag: `info`, `wallet`, `rollback`, `snapshot import`, `sync` and `import-block` read `network` /
  `network_dir` / `db_path` from `node.yaml` instead of always assuming mainnet. A relative `network_dir` is resolved against the
  config file's directory, so `sth-core info --config /srv/testnet/node.yaml` works from any cwd.
- `Network::from_dir` fails on a missing directory instead of silently falling back to the embedded mainnet files.

## [0.10.0]

### Added - Native tokens (typeGroup 3), dormant until milestone `tokens: true`
- Transactions TokenInit / TokenTransfer (1–64 recipients, memo ≤ 64 B) / TokenMint / TokenBurn: models (`TokenAsset`), wire format
  (serializer + deserializer, ids and signatures round-trip), stateless rules (`rules::check_token_format`: exact static fees, ranges,
  flags) and wallet-aware rules (`rules::check_token` over a `TokenView`: ticker SmartObject type 5 owned by the sender, ticker
  `^[A-Z0-9]{3,10}$`, balances, owner-only mint within `supplyCap`, burnable flag).
- State: `WalletState.tokens` (balances) and `WalletState.tokens_issued` (registry `TokenState` in the owner wallet) - covered by the
  existing undo/rollback; immutable indexes `tk:<id>` -> owner, `tks:<SYMBOL>` -> id (dropped when a TokenInit block is rolled back).
  50 % of the TokenInit fee is credited to the network burn address, the rest to the forger.
- Batch (`sync::check_stateful_rules`) and mempool views track pending token state; one TokenInit per id in the pool.
- Milestone fields `tokens`, `tokenFees {init, transfer, transferPerRecipient, mint, burn}`, `tokenTransferMaxRecipients` (default 64);
  defaults 100 / 0.1 / 0.01 / 1 / 0.1 STH.
- API: `GET /api/tokens`, `/api/tokens/{id|SYMBOL}`, `/api/tokens/{id}/holders`, `/api/wallets/{address}/tokens`;
  `attributes.tokens` / `attributes.tokensIssued` in wallets; `constants.tokens|tokenFees|tokenTransferMaxRecipients` in configuration.
- `configuration.constants` also exposes `tokenTickerSobjType` (5) and `tokenTickerRule` (`^[A-Z0-9]{3,10}$`) for wallet-side validation.
  Mainnet behaviour unchanged: group 3 rejected before `H_TOKENS`.

## [0.9.9]

### Added
- `sth-core snapshot import --strict` - **history audit**: every block of the dump is additionally passed through the live
  wallet-aware rules (`check_stateful_rules`: second signature, SHIP-13 SmartObjects) before being applied, so the whole chain can be
  replayed with exactly the checks the node applies to new blocks. Stops at the first block legacy accepted but we would reject.
- wallet-team guide for stage A (key derivation, commitment transaction, API, vectors, UX).

## [0.9.8]

### Added - Quantum Shield stage A (no consensus change; legacy nodes see ordinary transfers)
- `crypto::pq`: **ML-DSA-44 (NIST FIPS 204)** via the RustCrypto `ml-dsa` crate - deterministic key from the second passphrase
  (`seed = sha256("sth-pq-v1" || passphrase)`), pk 1 312 B, deterministic signatures 2 420 B with context `sth-pq-v1`, verify.
- **Commitment indexing**: a self-transfer (`recipientId == sender`, `amount ≥ 1`) whose `vendorField` is
  `sthpq1:<alg 2 hex>:<sha256(pq public key) 64 hex>` sets `wallet.pqCommitment {algorithm, commitment, height}`
  (last one wins; covered by undo/rollback like every wallet field). Foreign recipients and malformed strings are ignored.
- API: `/api/wallets/*` -> `quantumShield { committed, algorithm, commitment, height, active:false }`;
  `/api/node/configuration` -> `quantumShield { stage:"A", algorithms:[1], commitmentPrefix, activation:null }`;
  `/api/ntfry/metrics` -> `node.pqCommitments`; metrics page shows the number of committed wallets.
- Wallet-team vectors `tests/vectors/pq_v3.json` (passphrase -> seed -> pk -> commitment -> signature digest), tests `tests/pq.rs`.
- Stage B (v3 transactions with ML-DSA second signature, activation height) stays disabled until all active delegates run Rust.

## [0.9.7]

### Added
- Delegates dashboard: **legacy inference**. Live block intake now remembers, for the last 10 rounds, where each block first
  arrived from and who forged it. A delegate without a Rust announce whose observed blocks (≥ 2) all came through legacy peers
  that are not known Rust gateways is shown as `legacy` (inferred, with the block count); otherwise `unknown`.
  `/api/ntfry/delegates` adds `legacy`, `unknown` totals and per-delegate `inferred`, `observed`. `intake::origins()` /
  `intake::legacy_evidence()`; unit-tested.

## [0.9.6]

### Added
- **Delegates-on-Rust dashboard** (roadmap stage 1 progress). Rust nodes that forge publish a signed `Delegates` gossip announce every
  2 min: for each delegate key, an ECDSA signature over `sha256("sth-delegate-announce" || node id || timestamp)` - replay to another
  node or a stale (>10 min) announce is rejected, nobody can claim a delegate they do not hold. Every Rust node keeps a registry
  (`p2p_iroh::delegates::RustDelegates`, entries stale after 15 min).
- `GET /api/ntfry/delegates` - active delegate set (top-21 by votes) with `implementation: rust|unknown`, core version, node id,
  seconds since last announce; also in `/api/ntfry/metrics` -> `data.delegates {active, rust, list}`. Metrics page: progress bar
  "N of 21 active delegates on rust" + table.
- `delegate.announce` (default true) in node.yaml to opt out.
- Tests: `delegate_announce_is_bound_to_node_and_time`, `rust_delegates_propagate_over_gossip` (ignored, ~2 min, two nodes).

## [0.9.5]

### Fixed
- **Node stuck on a private fork after SHIP-13 height** (live report: local tip 11 802 295 `7940d64f` by nicholasflamel vs network
  `90d180d2`, 0 of 10 peers agree, catch-up looping on "peer blocks rejected"). Two root causes:
  1. `catch_up` had no fork handling - a `previousBlock` mismatch at tip+1 was treated as a bad peer and the same range was
     re-requested forever. It now rolls back (depth 1 -> 3 -> 9 ... ≤ UNDO_DEPTH) and resumes from the new tip; undo records are kept
     during catch-up when ≤ 2 000 blocks behind. Test `catch_up_rolls_back_private_fork`.
  2. The forger accepted a quorum of 1–2 peers (e.g. only our own iroh nodes echoing our tip while legacy peers were unreachable)
     and forged 5 blocks alone over an hour. New `delegate.min_quorum_peers` (default 3): the slot is skipped unless that many
     peers confirm our exact tip; the skip reason now says how many were needed / "refusing to forge in isolation".

### Added
- `sth-core rollback --to <height>` - manual rollback (stop the node first) for cases where automatic recovery is impossible.

## [0.9.4]

### Added
- **NETFORY n1 relays** built in: `https://relay-fsn7.sth.cx` and `https://relay-ru1.sth.cx` (iroh-relay) are always part of the
  relay map when `p2p.iroh.relay: true`. New options `relay_n0` (default true - keep the public n0 relays too) and `relays: [...]`
  (extra iroh-relay URLs). Startup log lists the relay map; `/api/ntfry/*` meta shows `relays` + `homeRelays`, the metrics page
  shows the connected home relay. `RelaySetup` in `p2p_iroh`; live test `n1_relays_reachable` (ignored).

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
- `/api/ntfry/metrics` -> `data.chain.multiPaymentLimit` and `data.chain.fees {transfer, multiPayment}` (smartoshi, current milestone).

## [0.9.1]

### Added
- Every transaction entering the mempool (REST `POST /api/transactions`, legacy `postTransactions`, iroh gossip) is logged as one
  line: `Received transaction Transfer S... -> S... amount=1.50000000 STH fee=0.10000000 STH nonce=7 id=... memo="..."` (INFO);
  rejected ones as `Rejected transaction ... : ERR_* reason` (WARN); full JSON at DEBUG (`RUST_LOG=sth_core::mempool=debug`).
  Type-specific summaries for Vote, DelegateRegistration, MultiPayment (total), SmartObject actions, HTLC.

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

### Added - SHIP-13 SmartObject transactions (consensus, activates at mainnet height 11 800 000)
- `network/mainnet/milestones.json`: `{ "height": 11800000, "sobj": true }` (SHIP-13 flag) - same value the legacy `@smartholdem/crypto-networks`
  release must carry. Before that height SmartObject transactions are rejected (`SmartObject transaction before sobj activation`),
  legacy business/bridgechain types (group 2, types 0–5) are never accepted (none exist in the chain; legacy disables them at the same milestone).
- Wire format bit-compatible with legacy `@smartholdem/core` crypto (`typeGroup 2`, `type 6`):
  `type u8 | subType u8 | action u8 | regIdLen u8 | registrationId | nameLen u8 | name | ipfsLen u8 | ipfsData`
  (`crypto/tx_serializer.rs`, `tx_deserializer.rs`; JSON asset `{ type, subType, action, registrationId?, data: { name?, ipfsData? } }`).
- Rules exactly as the legacy SmartObject handler (`rules.rs::check_sobj_format / check_sobj`): amount 0, exact static fee
  (register 50 STH, update/resign 5 STH - verified identical in the SmartHoldem fork), name `^[a-zA-Z0-9_!@$&.-]{1,40}$`,
  ipfsData base58 ≤ 128, network-wide unique `(name, type)` case-insensitive, Delegate SmartObject requires the sender's username,
  update/resign only by the owner with matching type/subType, no update/resign after resign. Legacy error names are kept.
- State: `WalletState.SmartObjects[<registrationId>] = { type, subType, data, resigned }` (`attributes.sobjects` in `/api/wallets`),
  `en:<name>-<type>` index for uniqueness/search; rollback restores wallets and drops the index of undone registrations.
- Mempool: activation gate, rules, one pending registration per `(name, type)` (`ERR_PENDING`).
- API: `GET /api/sobj` (filters type/subType/name/isResigned/address/publicKey/id), `GET /api/sobj/{id}`,
  `POST /api/sobj/search`; `/api/transactions/types|fees` switch group 2 to `SmartObject` after activation;
  `constants.sobj` in `/api/node/configuration`.
- Tests `tests/sobj.rs`: wire layout, activation gate, lifecycle/errors/state/rollback, batch register+update, mempool.
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
- **Signature verification 8× faster** (`tests/bench_block.rs`, 4 vCPU): block of 500 transfers verify 411 -> 52ms,
  10 000 transfers 8.2s -> 1.0s (verify + apply 9.1s -> 1.95s - a 10k-tx block now fits an 8 s slot).
  - `crypto/schnorr.rs`: the quadratic-residue test used `num-bigint` modpow on every signature (≈ 0.5ms); now k256's
    native `FieldElement::sqrt()`. `s·G − e·A` is one Shamir/Straus `lincomb_ext` instead of two scalar multiplications.
    Single-thread cost 0.82 -> 0.24ms per signature. `num-bigint` dependency removed (`k256` feature `expose-field`).
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
  a parking timer (30 s -> 60 s -> ... -> 10 min) that status probes do not lift. `best_for_blocks()` ranks by that metric,
  skips parked peers, and the workers rather wait 150ms for a fast idle peer than burn a timeout on a slow one.
- First `getBlocks` to a peer of unknown speed asks for 100 blocks (probe); subsequent requests are sized so the reply
  fits a 12 s budget (`blocks_limit()`, 50...400). Ranges are scheduled as `(from, to)`, so short replies re-queue only
  their tail - no gaps, no overlaps.
- `GET /api/peers` legacy entries show `blocksLatency`, `blocksLimit`, `blocksFailures`, `blocksParkedFor`.
- **Legacy peers reset the next connection from the same IP for ~1 s after a `getBlocks` reply** (`Connection reset
  without closing handshake`): the per-peer spacing is now measured from the previous reply (1.5 s) instead of the
  request start. Measured on mainnet from genesis (debug build): 0 resets, ~1 150 blocks/s vs ~150 blocks/s and
  50 resets/100 s before. The live follow loop treats such a reset right after connecting as expected (no WARN, no penalty).


### Fixed
- **Forging order was wrong** (`delegate/round.rs::shuffle`): the legacy `shuffleDelegates` loop is
  `for (i...; i++) { for (x < 4 && i < n; i++, x++) ... }`, i.e. the outer `i++` skips indices 4, 9, 14, 19 after every
  batch of four swaps. Our port swapped every index, so the node predicted the wrong slot and never forged
  (the network showed an empty slot for `nicholasflamel`). Verified against mainnet round 557584: 21/21 generators
  now match; regression test `shuffle_matches_mainnet_round_557584`.
- `delegate.enabled: true` without a passphrase no longer aborts the node with `config error: ... no secrets configured`:
  the problem is reported as an ERROR with a ready-to-paste `delegate:` snippet and the node continues as a relay.
  Secrets are resolved before peer discovery, so the message appears immediately.
- `peers.json` download: the full error cause chain is logged (DNS / timeout / TLS instead of just
  `error sending request`), one retry after 2 s, and the fallback line states how many built-in seeds are used.
- Live follow: the socket reset legacy nodes perform right after a `getBlocks` reply is no longer logged as
  `WARN getStatus failed, switching peer` and no longer penalises the peer; `following chain via legacy peer` is logged
  only when the peer actually changes.

### Changed
- Delegate identity is printed at start-up from the chain state, one line per key:
  `Delegate nicholasflamel: address S..., rank 12 of 21 (active), public key 02c9...`. When the database is empty or still
  syncing, the forger re-resolves username / rank every 30 s until the delegate is found.

## [0.8.0]

### Added
- `GET /status` - self-contained HTML status page on the local API (height, sync state, legacy + iroh peers with
  gateways, delegate slots / forging history, last blocks; refreshes every 5 s, no external assets).
- Gateway autodiscovery: `p2p.legacy_public_addr` ("ip:4001") is announced in the iroh `Peers` gossip message
  (`gateway` field, every 2 min); receivers add the gateway to their legacy peer table and list it in
  `/api/node/peers` meta `gateways` and per-peer `gateway`.

## [0.7.0]

### Added
- Forger quorum now includes iroh peers (`GetStatus` over `sth/rpc/1`) and the required share of agreeing peers is
  configurable: `delegate.quorum_share` (0.0–1.0, default 0.5, validated).
- NTP clock check at start (`src/ntp.rs`, SNTP to pool.ntp.org): `Your NTP connectivity has been verified ... Local clock
  is off by Nms`, WARN when the drift exceeds 1 s (slots are 8 s wide). Unit-tested against a mock SNTP server.
- Peer reputation sharing: every 5 minutes a node publishes its healthiest legacy peers (`GossipMessage::Peers`) on the
  iroh transactions topic; receivers add unknown IPs to their peer table with the reported height / latency / version,
  so freshly started Rust nodes skip the probing phase.

## [0.6.0]

### Added
- **Inbound legacy P2P server** (`p2p_legacy/server.rs`, `p2p.legacy_listen: "0.0.0.0:4001"`): nes/WebSocket handshake,
  ping, `p2p.peer.getStatus`, `p2p.peer.getPeers`, `p2p.blocks.getBlocks` (same `[u32 BE len][BlockHeader]*` codec as
  the legacy nodes), `p2p.blocks.postBlock` (chained -> verified -> applied, re-posts acknowledged, orphans rejected),
  `p2p.transactions.postTransactions` (-> mempool -> relay/gossip). Gateway nodes with a public IP become regular peers
  for old nodes; the same port can be published through netfory-provider (`local_ws_url: ws://127.0.0.1:4001`).
- `LegacyPeer::connect` accepts full `ws://` / `wss://` URLs, so `p2p.legacy_peers` may point at Rust nodes reached
  through a Web 4.0 proxy instead of `ip:4001`.
- Console numbers grouped like the legacy core (`Received new block at height 11,708,910 ...`, `Broadcasting block 11,708,910 ...`).
- `GET /api/node/forging` - delegate module state: configured delegates (username, rank, active), next own slot,
  last forged block (height, id, tx count, peers that accepted it), last 20 skipped slots with reasons.

## [0.5.0]

### Added
- **Iroh catch-up**: the download scheduler pulls 400-block ranges from iroh peers (fastest peer that has the range
  first, no per-second limit) alongside legacy IPs; `IrohNode::refresh_peers()` probes heights on start and every
  `p2p.refresh_secs`; `IrohNode::add_peer_addr()` / `MemoryLookup` for pinned LAN peers. Integration test: node B syncs
  two blocks forged by node A over iroh only.
- **Fast vote index** (`dv:<publicKey>` -> vote weight): updated incrementally on every wallet write and on rollback;
  `delegate_ranking()` is now O(delegates) instead of scanning all wallets; existing databases are migrated once at
  start (`ensure_vote_index`). `rebuild_vote_index()` for manual repair.
- Legacy-style console output: `Loaded N active delegate(s): name (publicKey)` (with a warning when the key is not
  registered or outside the top-21), `Next forging delegate ... is active on this node.`, `Forged new block ... by delegate ...`,
  `Broadcasting block H to N peers`, `Received new block at height H with T transactions from IP`,
  `Downloaded N new blocks ...`, `Blockchain 100% in sync`, `Starting Round R`.
- `GET /` -> `{ "data": "Hello World!" }` (legacy root probe used by netfory-provider) and legacy JSON 404 envelope for
  unknown routes.

- Forger quorum check: before forging, the best 8 legacy peers are asked for their tip; the node waits (until 2 s before
  the slot ends) for the previous slot's block to arrive and forges only when the majority reports our exact tip
  (height + id). Every owned slot is logged with the outcome (`Slot N belongs to delegate ...`, `Skipping slot N ...: reason`).

### Changed
- `P2pOptions` carries the optional iroh handle; block sources are `Source::Legacy(ip)` / `Source::Iroh(id)`.
- Live follow polls the best peer every 1.2 s instead of once per blocktime so new blocks are seen within ~1 s.

## [0.4.0]

### Added
- **Legacy API compatibility**: `GET /api/transactions/types`, `/api/transactions/schemas`, `/api/node/configuration/crypto`
  (network.json / milestones.json / exceptions.json / genesisBlock.json), `/api/peers/:ip`, `/api/votes`, `/api/votes/:id`,
  `/api/locks`, `/api/locks/:id`, `POST /api/locks/unlocked`, `/api/wallets/top`, `/api/wallets/:id/votes`,
  `/api/wallets/:id/locks`, `/api/sobj`, `/api/sobj/:id`, `/api/rounds/:round/delegates`.
- Full legacy filter set: transactions (`senderPublicKey`, `vendorField`, `version`, `sequence`, `timestamp/amount/fee/nonce`
  ranges), blocks (`generatorPublicKey`, `timestamp` ranges), wallets (`address`, `publicKey`, `balance`/`nonce` ranges,
  `orderBy`), delegates (`username`, `address`, `publicKey`, `isResigned`, numeric ranges, `orderBy`).
- `GET /api/node/peers` - sth-core extension: live health table of legacy and iroh peers (latency history, height lag,
  score, failures, ban state, source).
- **Iroh layer** (`p2p_iroh/`): endpoint + `Router`, gossip topics `sth/<nethash>/blocks` and `sth/<nethash>/transactions`,
  JSON RPC on ALPN `sth/rpc/1` (`GetStatus`, `GetBlocks`), gap-fill from the announcing peer, mempool bridge
  (gossip -> mempool -> legacy `postTransactions` and back). `node.yaml` section `p2p.iroh` (`enabled`, `secret_key_file`,
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
- `orderBy=...:asc` tests for blocks / transactions / wallets.

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
