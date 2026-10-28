# Changelog

## [0.25.49]

### Added mainnet milestone: Native Tokens + Token Market at height 12,365,000

- `network/mainnet/milestones.json` (embedded in the binary): from block **12,365,000** `tokens` (typeGroup 3),
  `sobjV2`, `strictBalance`, `tokenMarket { enabled, maxOpenOrders: 64 }` (SHIP-43), `minCoreVersion: "0.25.49"`.
- Token fees: defaults (TokenInit 500 STH, 50 % burned; transfer 0.1 + 0.01/recipient; mint 1; burn 0.1; sell 1;
  buy 0.1; cancel 0.1 STH) except **TokenMeta = 10 STH**.
- Legacy-incompatible: every active delegate must run ≥ 0.25.49 before the height.

## [0.25.48]

### Changed slot delay card shows milliseconds

- Delays under a second are printed as `34ms` instead of `0.0s` (`last 34ms  forged 34msx50  pull/legacy 7.6sx1`).

## [0.25.47]

### Fixed a delegate with a fast clock no longer steals the next delegate's slot (future-slot discard)

- `sync::apply_blocks` now discards a block whose slot starts more than 1 s in the future (`Error::FutureBlock`,
  `intake::FUTURE_SLOT_GRACE_MS`): not applied, not a peer fault (the follow loop retries after a second instead of
  parking the peer). Metrics `intake.futureRejected = { count, last: { height, generator, username, earlyMs } }`,
  alert `future-block` (names the delegate whose clock is ahead), slot delay card lists the culprit.

### Added active gap fill (a block that did not reach us is pulled, not waited for)

- Legacy `postBlock` for `tip+2 ... tip+8`: the blocks in between are pulled from the sender right away
  (`follow::pull_gap_from`), then the pushed block is applied instead of answering `not chained to our tip` and
  waiting for the follow loop (the `postBlock REJECTED ... 12205012 is not chained to our tip 12205010` lines).
- Forger quorum wait: when peers report a tip 1–3 blocks above ours, the previous slot's block is pulled from one of
  them (iroh first, then legacy) at once, one attempt per height, within the slot the slot is used instead of skipped.
- Minority rule (`delegate.forge_over_minority`, default true): if that pull failed, fewer than a third of the
  responding peers are ahead and a full quorum confirms our tip, the forger builds on our tip from mid-slot on (4–6 s
  into the slot) instead of skipping at the deadline the previous slot counts as missed, as in legacy core.
  If the minority's block spreads later it loses anyway: the next delegate builds on the majority branch, the minority
  rolls back one block. `false` restores the wait-for-everyone behaviour.

### Added missed forging slots per delegate

- `Storage::record_missed_slots`: gaps between consecutive block slots are attributed to the delegates that owned the
  empty slots in the round order (`rd:` snapshot + shuffle, same as the forger); `ms:<pubkey>` totals, `mb:<height>`
  records; undone on rollback, never double counted on re-apply; gaps longer than a round (outage) are not attributed.
  Counting starts when the node runs 0.25.47 (no historical rescan).
- API: `GET /api/delegates[/:id]` -> `blocks.missed`; `orderBy=blocks.missed`, `blocks.missed.from/to` filters.
  `/api/ntfry/metrics.delegates.list[].missed`, `delegates.missedSlots = { since, recent[50] }`; metrics page: `missed`
  column and a "missed slots" table.

### Added continuous clock drift detection (negative slot delays explained)

- node0 after 5 days of uptime showed `intake.avgDelayMs: -357` (`gossip/iroh -847`, `push/legacy -568`) while its own
  forged blocks measured +40 ms. A block cannot arrive before its slot starts: a negative delay means THIS machine's
  clock is behind the forgers' clocks (system time drifted, NTP sync off). The NTP check ran once at start only.
- `ntp::monitor`: SNTP probe at start and every 10 min (`ntp_server` in `node.yaml`, default `pool.ntp.org`); result in
  `/api/ntfry/metrics.clock = { server, offsetMs, checkedAt, ok, error, drifted }`. A failed probe keeps the last
  verdict.
- Telegram/webhook alert `clock-drift` when |offset| > 1 s once on entering the state, once on recovery.
- `intake.clockSkewHintMs`: earliest arrival among received blocks (ms before slot start) a lower bound on the local
  clock lag that needs no NTP at all. Metrics page: slot delay card turns yellow and says "blocks arrive up to X s
  before
  their slot -> this machine's clock is behind the network (enable NTP)" plus the live NTP offset.
- `doctor`: new `clock` check (FAIL on NTP offset > 1 s, WARN on early arrivals without NTP confirmation).

### Fixed same-height forks resolve in one hop; stall recovery never destroys finalized history

- Mainnet 2026-10-19/20: one delegate node was not receiving blocks within the slot and forged
  a competing block at the same height almost every round. Nodes that happened to get its block first kept it,
  stalled for 24 s, and the stall recovery escalated its rollback depth 1 -> 3 -> 9 rolling back **certified** blocks
  (soft finality) although the fork point was one block above.
- `intake::prefer_earlier_slot`: a same-height block from the EARLIER slot now replaces our tip immediately (same
  parent, tip has no children, tip not certified, signature valid); a later block never displaces an earlier tip. All
  nodes apply the same rule -> convergence in < 1 s, no stall, no deep rollback. Applied on both intake paths (gossip,
  legacy `postBlock`, which answers `status: true` after the swap).
- Stall recovery rollback is floored at the latest SHIP-35 certificate height: 15/21 delegates signed that block, the
  network has it, the fork is above.

## [0.25.44]

### Changed metrics page: a resolved fork no longer screams

- The forging card showed `⚠ SAME-HEIGHT FORK ... other side must roll back` until restart. Once finality has passed
  the height (or the chain is 21+ blocks beyond it) the line turns into a dim `fork at H (A vs B) resolved by the
  majority chain, finalized · 3h ago`, and disappears after 24 h. The wording no longer talks about legacy rollbacks.

## [0.25.43]

### Changed `sth-core init` defaults for an all-PQ network

- `discovery.listen_port: 4004` (was 0 = random): a fixed public UDP port gives stable direct dial info across restarts
  and makes relay-down mode effective. **Open UDP 4004** in the firewall.
- `p2p.legacy_listen: "0.0.0.0:4001"` (was empty): every node serves blocks over the legacy TCP port too the second,
  relay-independent transport that carried the chain through the gossip outage. **Open TCP 4001.** A bind failure is
  logged, not fatal. Existing node.yaml files are not changed set both values by hand.

## [0.25.42]

### Changed `SAME-HEIGHT FORK` alert names the culprit

- The alert now says which block came first and therefore whose node failed to spread it (`X's block (slot ...) did not
  reach Y within 8s, so Y forged the same height`), that the majority chain wins and the minority rolls back on its own,
  and what to check on X's node (gossip neighbours, `peers[].path = direct`, record on the PQ MAP, clock). The legacy
  rollback hint is gone there are no legacy forgers left.

## [0.25.41]

### Added relay-down mode (the network keeps working when every relay is unreachable)

- A relay outage (`Failed to connect to relay server: deadline has elapsed` on every node) froze NEW connections:
  gossip neighbours rotated out, wallets could not open sessions. The rejoin watchdog (every 30 s) now detects "no
  home relay connected for 60 s" -> WARN + alert `relay-down`, then every 30 s: pulls fresh sthpq records from PQ peers
  over the legacy TCP port (`p2p.pq.getAddrs`; the legacy table is seeded from peers.json no relay, DNS or DHT
  involved), pins every record with its **direct addresses only** (so iroh never waits on the dead relay path) and
  re-joins the gossip topics with those peers whenever a topic has fewer than 2 neighbours. `relay back: ...` + recovery
  alert when a relay reconnects.
- Prerequisite for this to help: a fixed public UDP port on servers (`discovery.listen_port: 4004`, open in the
  firewall; optionally `external_addr: "ip:4004"`) so records carry stable direct addresses across restarts.
  `ntfry.peers[].path = direct` confirms it.

## [0.25.40]

### Changed quieter gossip log

- `failed to send: connection task send loop terminated peer=...` is routine (a message aimed at a peer whose connection
  just closed, before the actor processed `PeerDisconnected`) and is now logged at debug level in the vendored
  iroh-gossip instead of WARN.

## [0.25.39]

### Fixed iroh-gossip#145 memory leak under peer churn (vendored patch)

- `SendLoop::run` used `Some(msg) = send_rx.recv()`: when the actor dropped a removed peer's sender the arm was
  silently disabled and the loop waited for the remote to close one leaked QUIC connection + driver task per rotated
  peer (reported upstream by the operator, still open). `None` now ends the loop and `connection_loop` closes the
  connection at once; `peer_topics` is pruned on `PeerDisconnected`.

### Changed forger wakes on the slot boundary

- Slots start on whole epoch seconds; the forger slept on a free-running 500 ms interval and noticed a new slot up to
  0.5 s late. It now wakes 25 ms after every second boundary (plus once mid-second): own blocks are forged ~0.25 s
  earlier on average, which is also what every other node's `gossip/iroh` delay inherits.

## [0.25.38]

### Fixed RSS growth 0.9 -> 3.1 GB in two days (then a frozen runtime)

- Mainnet log: `rss 935 MB` -> `rss 3084 MB` 48 h later with open fds flat and no in-memory structure growing the
  classic glibc malloc pattern: one arena per thread (tokio workers + the blocking pool used for every block apply /
  sled read), fragmented memory never returned to the OS. The binary now uses **mimalloc** as the global allocator
  (`#[global_allocator]`), which keeps RSS close to live data and returns freed pages.
- The 10-minute `memory:` line now also prints `tasks alive`, runtime `workers` and gauges registered by the
  subsystems (`iroh peers`, `finality votes`, `rpc pool`, `mempool banned peers`) so a real structure leak would be
  visible at a glance (`mem::gauge(name, || size)` for new ones).
- Builds on 0.25.37 (pooled RPC connections, relay queue 4096, rate-limited relay warnings).

## [0.25.37]

### Fixed relay overload (`Dropping received relay packet: no available capacity`, hundreds per second)

- Every iroh RPC (`GetStatus` probes before each forged block, the 60 s peer refresh, block pulls) opened and closed a
  fresh QUIC connection: a handshake + holepunching each time, with the first packets always through the relay. RPC
  connections are now pooled per peer and reused (one QUIC stream per request; a stale connection is replaced on the
  first failed `open_bi`), so after the first holepunch everything goes direct and the relays only carry what they must.
- Vendored iroh: relay receive queue 512 -> 4096 datagrams (bursts no longer drop QUIC packets), and the drop warning
  is rate-limited to one line per 5 s with a count per-packet lines (synchronous stdout writes on the runtime)
  made the overload worse.
- Metrics `ntfry.peers[].path` = `direct` / `relay` for each iroh peer: shows at a glance whether holepunching works (a
  node whose peers are all `relay` needs its UDP port opened `discovery.listen_port` + firewall).

## [0.25.36]

### Fixed runtime stalls on 1–2 vCPU hosts

- A 2 vCPU / 2 GB node logged `tokio runtime has not ticked for 19s`: with `#[tokio::main]` the runtime had 2 workers,
  so two blocking operations at once (sled read on a cold page cache, a page swapped back in) froze every async task.
  The runtime now always has at least 4 workers (`max(cpus, 4)`, thread name `sth-worker`).
- The freeze-watchdog warning now prints the pressure summary (`rss`, `swapped out`, `loadavg`, processes blocked on
  I/O) so memory/disk pressure is told apart from a deadlock; the 10-minute memory line shows swapped-out MB;
  metrics `node.swapBytes`.

## [0.25.35]

### Added open-files visibility (`Too many open files`)

- `axum::serve: accept error: Too many open files (os error 24)` on a public node = the process hit its soft
  `RLIMIT_NOFILE` (1024 by default outside the shipped systemd unit). The node now warns at start when the limit is
  below 8192, logs `open fds N / limit` with the 10-minute memory line and exposes `node.openFds` / `node.fdLimit` in
  `/api/ntfry/metrics`, so a real descriptor leak (count growing without bound) can be told from a low limit.

## [0.25.34]

### Changed `sth-core id`

- `sth-core iroh-id` is renamed to `sth-core id` (`--url`, `--name`, `--key-file` unchanged); `iroh-id` stays as an
  alias so existing scripts keep working. Docs updated.

## [0.25.33]

### Changed finality votes are event-driven

- `publish_finality` subscribes to `Storage::subscribe_chain` (same source as the SHIP-44 wallet push stream and the
  gossip block publisher): own votes leave within milliseconds of a block being applied instead of at the next 500 ms
  tick, so the 15/21 certificate forms ~0.25 s earlier on average. The tick stays for the one-slot resend / safety net.

## [0.25.32]

### Changed faster slots: blocks reach the network ~1s after the slot starts (was ~5s)

- Forger quorum check (`wait_for_quorum`): status probes to legacy peers + iroh neighbours now stream in
  (`FuturesUnordered`) and the check returns as soon as enough peers counted against everyone asked, so silent
  peers never relax the bar confirm our tip and nobody is ahead. Before, `join_all` waited for the slowest probe:
  one unreachable peer cost the full 4 s timeout on every slot (mainnet `intake.forged.avgDelayMs ≈ 5000`).
- Gossip block publisher is event-driven (`Storage::subscribe_chain`): a block forged or received over the legacy port
  is on the iroh wire within milliseconds instead of at the next 2 s poll (the poll stays as a safety net).
- Net effect with imperfect links: a block now has ~7 s of the 8 s slot to spread instead of ~3 s.

## [0.25.31]

### Fixed premature `dht-unreachable` alert after a restart

- Every node alerted `Mainline DHT unreachable ... after 1 probe(s)` 90s after the 0.25.30 rollout, including hosts
  where the DHT is known to work: a freshly published record needs minutes to spread, so the first own-record
  lookup fails. The probe now retries after 5 min (then 15 min) and alerts only after 3 consecutive failures; status
  stays `probing` until then. Once reachable, probes go back to every 30 min.

## [0.25.30]

### Fixed network-wide gossip stall (finality lag, blocks only via legacy port)

- Observed on mainnet: every PQ node kept the chain over the legacy port only (`intake.bySource` without `gossip/iroh`),
  finality stuck at 11,996,756 (8–10 of 15 votes at the tip), `finality.lastError = "broadcast timed out after 5s"`,
  thousands of `broadcastFailed`; relay warnings `Dropping received relay packet: no available capacity`. Root cause in
  iroh-gossip 0.101: the gossip actor awaits `active_send_tx.send(msg)` per peer (queue of 64); one slow / frozen
  neighbour (a delegate node had frozen for hours) stops reading, its queue fills and the single actor blocks for EVERY
  peer and topic no broadcasts, no incoming messages, until that connection dies by itself (n0-computer/iroh-gossip#47
  "backpressure leads to unresponsive states").
- iroh-gossip 0.101.0 is now vendored (`vendor/iroh-gossip`, `[patch.crates-io]`) with three patches:
    1. the actor uses `try_send`: a full per-peer queue drops the message for that peer only (plumtree IHave/Graft
       recovers it) instead of freezing the actor;
    2. the per-connection send loop bounds every write with `SEND_TIMEOUT` (10 s) and a failed send loop closes the QUIC
       connection, so the actor gets `PeerDisconnected` and hyparview replaces the slow neighbour;
    3. n0-computer/iroh-gossip#172 (reported by the operator): quitting one topic no longer closes a connection other
       topics still share restarted peers are not ignored for ~60s.

## [0.25.29]

### Added runtime freeze watchdog (`freeze_abort_secs`)

- A delegate node froze completely (no log lines after `Dropping received relay packet: no available capacity`, its
  delegates missed every slot until a manual restart). That iroh warning is a symptom: the relay actor could not hand
  packets to the endpoint because the tokio runtime was no longer being polled. A plain OS thread now watches a 1 s
  heartbeat of the runtime; after `freeze_abort_secs` (default 120) without a tick it logs `RUNTIME FROZEN ...`, sends
  the `freeze` alert on its own mini-runtime and aborts the process so systemd (`Restart=always`) brings the node back.
  At 15 s of lag it prints a hint with the `gdb -p ... thread apply all bt` command for a stack dump. `0` = never abort.
  Metrics: `node.runtimeLagSecs`.
- `alerts::send_now` immediate delivery shared by `notify` and the watchdog thread.

## [0.25.28]

### Fixed false `CHAIN STALLED` / `chain recovered` pairs

- The stall watchdog compared peer heights with a tip read one slot (8 s) earlier: when the network skipped slots and
  the next block arrived during that sleep, the peer that had just served it looked "1 ahead" of the stale tip ->
  ERROR + Telegram alert, then "recovered" 8s later. The tip is now read right
  before the comparison.

## [0.25.27]

### Changed `dynamic_fees.addon_bytes.ntfry`

- The type-5 (Netfory content pointer) addon is spelled `ntfry` in node.yaml.

### Added `node-started` alert

- On every start the node sends the startup banner as plain text (`NODE STARTED · sth-core-pq vX.Y.Z`, network, last
  block, modules on/off, delegate keys, state) to Telegram / the webhook one message per node, tagged with its label.

### Added `alerts.node_name`

- Every alert is tagged `[sth-core-pq <node_name | hostname> <legacy_public_addr> <network unless mainnet>]` so several
  nodes reporting into one Telegram chat can be told apart (`alerts.node_name: "fsn-sth-01"`, default = hostname).
  `alerts-test` prints the label before sending.

## [0.25.26]

### Added `mempool.antispam` (pool policy, not consensus)

- New module `antispam` wired into the pool: `max_per_sender` / `max_bytes_per_sender` (pending per address, a
  same-nonce
  replacement does not count), `peer_tx_per_min` budget per legacy IP / iroh EndpointId (the rest of a batch is
  dropped),
  `peer_invalid_per_min` bad signatures / bytes / network (nonce faults are NOT counted) -> peer banned for `ban_secs`
  (its transactions are dropped before
  any verification; blocks are unaffected), `deny_peers` (always dropped), `replace_by_fee` (a full pool admits a
  higher fee-per-byte transaction by evicting the cheapest pending ones only a sender's LAST nonce, never the newcomer's
  own sender), `alert_drops_per_min` -> alert `spam-wave`. Rejection code `ERR_SPAM_SENDER`.
- `Mempool::add_many_from(txs, TxOrigin)` legacy `postTransactions` and iroh gossip pass the peer; REST stays `Local`.
- Metrics `mempool.antispam { enabled, config, counters{dropped,rateLimited,senderLimited,denied,bannedDropped,evicted,bans},
  banned[{peer,secondsLeft,invalid,dropped,bans}], topSenders[{address,pending,bytes}] }` + a line in the mempool card.

## [0.25.25]

### Added live `audit-all` on a running node + `alerts-test`

- `sth-core audit-all [--fix]` no longer needs `systemctl stop`: when the sled directory is locked by the node the CLI
  asks the node to audit itself (`POST /api/ntfry/audit?fix=...`, `GET /api/ntfry/audit` for progress loopback only,
  proxied requests refused) and follows the job. `Storage::audit_all_live`: the chain walk runs without any lock (blocks
  keep being applied, a delegate keeps forging), the apply lock is held only for the tail blocks applied
  meanwhile are booked, all wallets compared, drifted ones repaired + vote index rebuilt in the same critical section.
  Aborts with "chain reorganized during the audit" when the anchor block changed (fork rollback). Summary lands in
  `node.lastAudit` / metrics card / `doctor` as before; drift alert `audit-all (live) ...`. `--api URL` for a
  non-default API address. Offline mode (node stopped) is unchanged.
- `sth-core alerts-test`: validates the bot token (`getMe`), with an empty `telegram_chat_id` lists the chats the bot
  has seen via `getUpdates` (private chat = your user id, groups negative) ready to paste into node.yaml, otherwise
  sends
  a test message to Telegram and the webhook and explains Telegram's refusal (`chat not found` -> send the bot
  `/start`).

### Fixed static security audit before the all-delegates migration

- Legacy P2P `postBlock` / `getBlocks` replies: the header's `numberOfTransactions` was used for `Vec::with_capacity`
  before any check a peer could make the node reserve gigabytes (abort under memory overcommit limits). Bounded by the
  milestone `maxTransactions` and by the payload size before allocating; client-side decode no longer pre-reserves.
- Legacy P2P `postTransactions`: capped at 40 transactions per request (legacy `maxTransactionsPerRequest`) so a peer
  cannot burn CPU on unbounded ML-DSA verification; WebSocket message / frame size capped at 20 MiB (legacy
  `maxPayload`) instead of tungstenite's 64 MiB default.
- Live audit: the full wallet comparison now runs UNLOCKED too; under `apply_lock` only the tail blocks are booked and
  the candidates (unlocked drift + wallets touched by those blocks) re-compared -> the lock is held for milliseconds,
  not for a full `w:` scan.
- `forger`: `&id[..8]` on a short block id could panic -> `get(..8)`.

## [0.25.24]

### Added `same-height-fork` alert (fork of 2026-09-29 at 11,988,289)

- Bisection PQ vs legacy showed the split was NOT double forging: delegate (PQ) forged #11,988,289, the legacy nodes
  never applied it and titan (legacy) forged its own #289 one slot later; the legacy branch never re-converged. The
  double-forge guard now treats a VALID block by another delegate at a height we already have as a network fork:
  ERROR `SAME-HEIGHT FORK at H ...` + alert `same-height-fork` (with the rollback height for the other side), red marker
  in the forging card, `doubleForgeGuard.forks[]` in metrics. Signature is verified first; no yield, no consensus
  change.

## [0.25.23]

### Fixed DHT behind ISP filtering (first mainnet run in RU)

- `n0_mainline` logged `ERROR Could not bootstrap the routing table` every 2s when outbound UDP to the public
  BitTorrent bootstrap routers is filtered (common for Russian ISPs); `iroh_mainline_address_lookup` logged every
  resolve at INFO. Both targets are muted by default (`RUST_LOG` overrides). The node now probes the DHT itself:
  resolves its own record 90 s after start and every 30 min -> `ntfry.discovery.dht {status: probing|ok|unreachable}`,
  one WARN `Mainline DHT unreachable ...` + alert `dht-unreachable`, PQ MAP card turns `dht blocked`.
- `discovery.mainline_server: true` runs the DHT socket in server mode so a gateway becomes a bootstrap node for the
  network (`mainline_bootstrap: ["<ip>:<mainline_port>"]` on the others) the way around filtered public routers.
- Docs: NETWORK-AUTONOMY (DHT health, own bootstrap nodes, diagnostics) and (SIGINT / systemd / setsid).

## [0.25.22]

### Added

- **SHIP-45** `https://github.com/smartholdem/SHIPs/blob/master/SHIPS/SHIP-45.md` Network Autonomy: double-forge guard,
  relay guard, sthpq address book, Mainline
  DHT discovery, n0-free mode (Requires 4, 35, 44); indexed in
  `https://github.com/smartholdem/SHIPs/blob/master/SHIPS/SHIP-45.md`.
- Metrics page tab **PQ MAP**: every PQ node from the address book with signed addresses, relay, version, source, last
  answer, record age and state (neighbour / fresh / stale / unverified), merged with live iroh peer height/latency; live
  peers without a record are listed too. `ntfry.discovery.peers` grows to 200 entries.

### Changed

- WebSocket bridge (SHIP-44) no longer builds an n0 endpoint: remote targets are dialled through the node's main
  endpoint (address book + DHT + n1 relays), the local target through a `Minimal`, relay-less client endpoint with the
  node's own sockets pinned. The bridge has no n0 dependency left.

## [0.25.21]

### Added Mainline DHT address lookup (`p2p.iroh.discovery.mainline`, on) + n0-free mode

- `iroh-mainline-address-lookup` 0.5 / `n0-mainline` 0.6: the node publishes/resolves pkarr `SignedPacket`s as BEP-44
  mutable items keyed by its Ed25519 EndpointId (same scheme as NETFORY clients). Only relay URLs and public IPs are
  published (custom `AddrFilter`). `mainline_bootstrap` adds own seeders to the public routers, `mainline_port` pins
  the DHT socket.
- `discovery.n0_dns: false` builds the endpoint with `presets::Minimal` (no n0 DNS publish/resolve, no n0 relays):
  discovery = sthpq address book + Mainline DHT only. Default stays `true` (fallback) until operators confirm the DHT.
- `docs/NETWORK-AUTONOMY.md`: full guide to the four new modules and the n0 phase-out; `README.md` index entry.

## [0.25.20]

### Added sthpq address book (`p2p.iroh.discovery`, module on by default)

- `p2p_iroh::addrbook`: `SignedAddr` records (EndpointId, public `ip:port`s, relay, `seq`) signed by the node key and
  bound to the nethash; exchanged over a dedicated gossip topic `sth/<nethash>/addrs` (own record + 16 fresh peers,
  Bitcoin `addr` style), RPC `GetAddrs`, and the legacy port route `p2p.pq.getAddrs` (PQ peers only). Records feed
  iroh's `MemoryLookup` (direct dial without n0 DNS) and persist in sled (`ab:`). Liveness follows Bitcoin addrman:
  never answered after 3 tries (known > 1 day) or silent > `expire_days` (30) -> evicted; crawler every 5 min.
- `bootstrap` entries accept `<EndpointId>@<ip>:<udp port>`; `discovery.listen_port` (fixed UDP) and
  `discovery.external_addr` give gateways stable direct addresses. `IrohNode::spawn_with(..., DiscoveryOpts)`.
- Metrics: `ntfry.discovery`, `ntfry.directAddrs`.

## [0.25.19]

### Changed relays

- `p2p.iroh.relay_n0` defaults to **false**: the public n0 relays are rate-limited ("not for production") and won the
  latency race against the NETFORY n1 relays, then dropped wallet handshakes (`remote=relay euc1-1.relay.n0.iroh.link
  ... timed out`). WARN at start when it is still `true`; `doctor` warns when a home relay is n0.

### Added relay guard (`p2p.iroh.relay_guard`, module on by default)

- Counts wallet-API handshake timeouts per incoming relay; a non-protected relay (not n1 / `relays`) exceeding
  `max_failures` (5) within `window_secs` (600) is removed from the relay map at runtime (`Endpoint::remove_relay`) so
  the home relay migrates to n1. WARN + alert `relay-degraded`; `ntfry.relayGuard` in metrics.

## [0.25.18]

### Added double-forge guard (`delegate.guard`, module on by default)

- Root cause of the 2026-09-23 split (fork at 11,926,030): one delegate passphrase loaded on a legacy and a PQ node.
  The guard detects a block signed by OUR key arriving from the network (ERROR `DOUBLE FORGING RISK` + alert
  `double-forge`) and two blocks at one height by one generator (signature verified, ERROR `DOUBLE FORGING by`).
  `mode: yield` (default) skips that key's slots while foreign blocks keep coming (`rounds: 2` window, alert
  `double-forge-yield`); `mode: alert` only reports. Metrics `doubleForgeGuard`, metrics page markers

## [0.25.17]

### Added `sth-core snapshot dump [--out DIR] [--end HEIGHT]`

- Writes `<out>/1-<end>/{meta.json, blocks, transactions, rounds}` + `<out>/1-<end>.tgz` in the legacy core-snapshots
  layout (gzip streams of `[u32 LE len][record]`, blocks = serialised header with signature, transactions = msgpack
  `[id, blockId, height, sequence, timestamp, bin]`) straight from sled. Readable by `sth-core snapshot import` and by
  the legacy `snapshot:import`. `--end` below a fork point shares a clean chain. Round-trip verified (dump -> import ->
  identical tip id, signatures checked).

## [0.25.16]

### Added `sth-core block --height H [--height H2 ...]`

- Offline lookup of stored blocks (id, generator, time, tx count, previous) plus `rollback possible down to: N`
  (`Storage::oldest_undo_height`) compare with a legacy node's `/api/blocks?height=H` to find the common block before
  `sth-core rollback --to`.

## [0.25.15]

### Added legacy network split alarm

## [0.25.14]

### Changed

- Startup banner: transaction type 5 is listed as `Ntfry`.

## [0.25.13]

### Fixed REST sync died with `stale tip` when the chain advanced through iroh

- With `p2p.legacy_enabled: false` blocks arrive over iroh gossip (and from our own forger) while the REST syncer is
  still fetching its batch; applying the batch then failed with `stale tip: block N cannot be applied: chain tip is M`
  and `sth-core` exited. The syncer now re-anchors on the local tip before applying, drops the blocks it already has,
  and on a `StaleTip` race applies only the remaining ones (`batch ... already applied through another path`). The
  follow loop also refreshes the tip from storage every poll.

## [0.25.12]

### Fixed SHIP-35 finality stalls on a node holding many delegate keys

- Vote batches are now sent in chunks of 8 (`FINALITY_VOTES_PER_MESSAGE`): a JSON vote is ~320 bytes, so 13+ keys on
  one node produced a `Finality` gossip message above the iroh-gossip 4 KiB default and it was never delivered (the
  error was a DEBUG line). Our own gossip limit is raised to 64 KiB (`GOSSIP_MAX_MESSAGE_SIZE`) for blocks with
  transactions from newer peers.
- The publisher no longer blocks: each broadcast has a 5 s timeout; failures/timeouts are WARN and counted
  (`finality.broadcastFailed`, `lastError`). Own votes are always counted locally first, so a node with ≥ quorum keys
  certifies even when gossip is down.
- Observability: `finality.ownVotesAtTip`, `lastOwnVoteAge`, `lastOwnVotes`; metrics page shows `own N votes, signed Xs
  ago` (red when stale); `doctor` gets a `finality` check (FAIL when own votes are stale while blocks arrive, WARN when
  no key is in the active set). WARN (once a minute) when none of the loaded keys is in the round's active set.

## [0.25.11]

### Changed iroh on by default

- `sth-core init` / `NodeConfig::default()` now write `p2p.iroh.enabled: true` with the three mainnet PQ bootstrap
  EndpointIds (`MAINNET_IROH_BOOTSTRAP`: 747a93f8..., d8c32e15..., 9906e9f2...), `serve_blocks: true`, `relay: true`.
  `sth-core init --newnet` keeps iroh off with an empty bootstrap list. Comment in node.yaml explains iroh.key.

## [0.25.10]

### Changed

- `postBlock` answered with `status:false` and the peer's tip **already at our height** now counts as delivered (the
  block reached the peer first through another relay a PQ peer that got it over iroh gossip); it was logged as
  "REJECTED (invalid transaction?)" on every peer of a broadcast and undercounted `accepted`.
- Default log filter also mutes `noq_proto` WARN `failed closing path err=LastOpenPath` (QUIC path idle timer on a
  connection's only path transport noise on `sth/rpc/1` sessions).

## [0.25.9]

### Added console highlights for the forger + `--color`

- `console.rs`: one coloured stdout line for the events an operator watches: `▌ 18:01:21  FORGED    #11,916,646  paracelsus
  ·  0 tx · fees 0 STH · reward 2 STH · id 61eb58cfa0...` (blue), `RELAYED  #... · 14/15 legacy peers accepted` (green),
  `SKIPPED  slot ... delegate · reason` (amber), `REJECTED #... · none of N legacy peers accepted` (red). The tracing
  lines
  stay unchanged for log files; highlights print only when colour is on.
- Global `--color auto|always|never` (default auto = stdout is a terminal, honours `NO_COLOR`): drives tracing ANSI,
  the banner colours and the highlights. Under pm2 use `--color always` (pm2 keeps ANSI in `pm2 logs`).

### Changed `postBlock REJECTED` wording

- A legacy peer answering `{ status: false, height: <its tip> }` while it is behind us (e.g. `peer_height 11,916,659`
  for our `11,916,665`) simply cannot chain the block yet: now INFO "the legacy peer is behind the network ... (peer
  problem, not ours)" with `behind = N`. Peer already at/after our height -> INFO "already has this height". WARN with
  the `audit-wallet` hint stays for a peer at the right height rejecting a block **with** transactions; an empty block
  rejected at the right height points at the peer's log (slot / generator / version check).

## [0.25.8]

### Added wallet-api QR code at startup

- `p2p.iroh.wallet_api.startup_qr` (default `true`): after the banner the node prints its `api://<EndpointId>/sth-core`
  URL as a half-block QR code (version 4, 18 lines) so a mobile SmartHoldem wallet pairs with the node by scanning.
  Dependency `qrcode` (no default features).

## [0.25.7]

### Added banner section "SHIP-44 Wallet API on iroh"

- After **State**: protocols (`netfory/api/1 (REST) + sth/events/1 (push subscriptions)`), the node's
  `api://<EndpointId>/sth-core` URL and the limits (`max_connections`, `rate_limit_rps`, WS bridge address). Shows the
  reason when off (`p2p.iroh.enabled: false` / `p2p.iroh.wallet_api.enabled: false`).

## [0.25.6]

### Changed startup counters

- Registered delegates are always counted for the banner (`wu:` index, milliseconds) and shown as
  `N registered · 21 active per round`; `startup_scan` now covers the wallet scan only and defaults to `true` (~1–3 s on
  mainnet). Both are informational the node never regenerates state at start (sled keeps it incrementally; integrity
  is `sth-core audit-all`).

## [0.25.5]

### Changed startup banner is now a framed, coloured box with the STH logo

- `banner.rs` prints on stdout: ASCII `STH` logo + "SmartHoldem / Post Quantum CORE / vX.Y.Z", then a box (≤ 99 cols,
  long values wrap at " · ") with sections: node facts (network, database, last block, milestone, consensus, fees,
  Quantum Shield), **Modules** with ●/○ switches (P2P legacy, P2P iroh, Wallet API, WS bridge, REST API, metrics page,
  delegate, Service Nodes, Token Market, alerts, stall watchdog), **Transaction types** by group, **State** (delegates,
  wallets, mempool). ANSI colours only when stdout is a terminal (amber titles, green ● on, dim ○ off); plain text
  under pm2 / journald. Tests: rows, aligned box width with and without colours, wrapping, logo.

## [0.25.4]

### Changed startup banner no longer scans every wallet by default

- New `startup_scan` (node.yaml, default `false`): the wallet / registered-delegate counters of the banner needed a
  full `w:` + `wu:` key scan (seconds on mainnet with a cold cache and it delayed forging after a restart). Off ->
  `State: delegates / wallets not counted (startup_scan: false) · ...`; `startup_scan: true` restores the counts.

## [0.25.3]

### Changed "Accepting incoming connection ended with error: timed out" now says who knocked

- The iroh router logged incoming wallet-api connections that never completed the QUIC handshake as WARN with only
  our own id (`me=...`) operators read it as the node dialling itself. `iroh::protocol` is now filtered to ERROR by
  default (`RUST_LOG` overrides), and `wallet_api::accept_with_log` (`on_accepting` of `netfory/api/1` and
  `sth/events/1`) logs the same event at INFO with the remote (`ip:port` or `relay <url> <peer>`), the ALPN and the
  error: "client gave up or its relay path is broken not a node fault".
- `Stats.handshake_failed` + a ring of the last 20 incoming connections (`recentClients { atUnix, alpn, remote, peer,
  outcome }`) in `irohWalletApi` (`/api/node/status`) and `ntfry.walletApi` (`/api/ntfry/metrics`); metrics page gets a
  "wallet api" line (connections, sessions, requests, events, handshake timeouts, last clients).

## [0.25.2]

### Fixed `delegate.secrets: "./delegates.json"` was rejected

- `secrets` expected a YAML list of passphrases, so a legacy file path given there failed with `invalid type: string,
  expected a sequence`. `secrets` now accepts a single string as well, and any entry ending in `.json` is read as a
  legacy `delegates.json` (`{ "secrets": [...] }`) the same as `secrets_file`. File errors name the path and the
  expected format (`cannot read delegates file ...`, `expected the legacy format ...`, `no passphrases in "secrets"`).
  Test: `delegate::tests::secrets_accept_a_legacy_delegates_json_in_either_field`.

### Fixed repository hygiene

- Root `.gitignore` had 180 auto-appended `sth-core-pq/target/debug/...` paths and the nested `target/` rule did not
  match the `target` symlink that 0.25.0 briefly committed. Now: `sth-core-pq/target`, `**/target/`,
  `sth-core-pq/data/`.

## [0.25.1]

### Added legacy-style startup banner

## [0.25.0]

### Added fee advisor (legacy pools refuse the fee -> only PQ delegates forge)

- `mempool.legacy_dynamic_fees` (node.yaml, default `false`): the legacy Core 3.8.2 peers take exactly the static fee (1
  STH per transfer); `true` = they run dynamic fees and the SHIP-41 minimum is their floor.
- `Mempool::fee_hint(tx)` -> `{ fee, legacyMinFee, legacyDynamicFees, message }` when the legacy pools will refuse the
  fee. Surfaced in `POST /api/transactions` (`feeHints: { id: hint }`, additive top-level key), in
  `GET /api/transactions/unconfirmed` (`pool.feeHint`), in the wallet stream `mempoolTx` frame (`feeHint`, also over
  the WS bridge) and in the node log (`fee advisor: legacy pools will refuse this fee`).
- Metrics: `mempool.legacyPool { dynamicFees, minTransferFee, pendingRefused }`; mempool card shows the legacy floor and
  a warning with the number of pending transactions the legacy pools refuse.

### Added stall cause on the metrics page

- `chain.stall.lastReason { kind, peer, detail, height, atUnix }` the last diagnosis made by the follow loop or the
  watchdog: `lagging-peer` (followed peer stuck, switched), `fork` (rolled back, with the peer that served the other
  branch), `pull-broken` (tip on the network branch, no block from the follow loop; peer that confirmed the branch),
  `iroh-pull-failed` (iroh peer id + error), `no-legacy-peers`, `rollback-failed`.
- `chain.stall.lastStall { height, seconds, networkHeight, recoveredHeight, endedAtUnix }` the previous stall kept
  after recovery. Page: "last stall cause" in the last-block card + a detail line under the doctor verdict; `doctor`
  prints the cause in its STALLED check.

## [0.24.3]

### Fixed "stall recovery: tip is on the network branch but no block arrives pull path broken"

- `follow_live` picked the best legacy peer once per session and then polled only that peer. When the followed peer
  stopped advancing (a lagging legacy node or another stalled PQ Node reporting `height ≤ our tip`), the loop kept
  asking it every 1.2 s forever while the rest of the table was blocks ahead -> CHAIN STALLED with a correct tip and
  no block arriving. The loop now checks `best_height()` after every `getStatus`: followed peer not ahead of us while
  the table is ≥ 2 blocks ahead for longer than one slot -> WARN `followed peer lags behind the network, switching block
  source`, the peer is parked as a block source for 60 s (`PeerTable::park_blocks_for`, no failure counted) and the
  best peer is re-selected. Tests: `follow::tests::lagging_peer_*`, `health::tests::park_blocks_for_*`.

## [0.24.2]

### Fixed PQ Nodes rejected every `postBlock` that carried transactions

- `LegacyServer::decode_full_block` handed the whole payload (header ‖ u32-LE lengths ‖ transactions) to the strict
  header parser -> "block N: 160 unexpected trailing bytes after header" (160 per transaction) -> status 400 to the
  forging
  peer, and the block arrived a second later via iroh gossip instead. New `deserialize_block_header_prefix` returns the
  header and its length; `decode_full_block` continues from there. Regression in `tests/legacy_server.rs` (block with a
  transaction accepted, wallet nonce advanced). Legacy peers were never affected (they use their own parser).

### Added did the legacy pools ACCEPT the relayed transactions?

- `LegacyPeer::post_transactions` returns the number of accepted ids (legacy answers a JSON array); `RelayCoverage`
  gains `legacyAccepted` / `pqAccepted`. Delivered-but-refused (`legacy 12/12 accepted 0`) means the legacy pools
  rejected the transaction typically a fee below their minimum (legacy pool rule, not consensus: the block-level rules
  still accept it, so only PQ delegates forge such transactions). WARN in the relay log + red hint in the mempool card.

## [0.24.1]

### Fixed "postBlock REJECTED ... peer_height=0" hid the peer's reason

- `LegacyPeer::post_block` decoded an error frame (HTTP-like status 400 + text) as a default `{status:false, height:0}`;
  it now returns `Err` with the peer's text, and the forger logs it (`postBlock REJECTED / failed the peer's reason is
  in error`). A PQ Node that rejects a block logs WARN `postBlock rejected (drifted wallet state? run audit-all --fix)`.

### Fixed `doctor`, round-1 snapshot

- Fresh networks (and `active_delegates: 1` test nets) did not forge: the round-1 seed at height 1 skipped the boundary
  check. Height 1 now seeds round 1 and still runs the `% n` boundary logic.
- Tries the REST API and then the metrics listener (`api.metrics_listen`), prints the OS-level cause (e.g. `Connection
  refused`) and honours `--api`; no proxy env. `peers --accepted` / doctor acceptance now use the `getPeers` answers
  collected
  by the probe (legacy refuses a second connection from the same IP for ~1 s, so re-asking got "1 / 1 peers" only) —
  `PeerStats.accepted_peers`.

### Added doctor verdict on the metrics page, relay coverage (legacy vs PQ)

- `/api/ntfry/metrics.doctor{verdict,level,checks[]}`: the in-process part of `sth-core doctor` (`doctor::summary`)
  evaluated
  on every metrics call; the page shows a `HEALTHY / DEGRADED / UNHEALTHY` pill under the sync badge with the non-OK
  checks (`data-testid="doctor-verdict"` / `doctor-detail`). Legacy acceptance still needs the CLI (network round-trip).
- PQ Nodes recognise each other: `getStatus.config.plugins["@smartholdem/core-pq"]` marker (legacy ignores unknown
  plugin
  keys); `PeerStats.pq`, `PeerTable::alive_by_kind / is_pq`, `pq` flag in the ntfry peer list.
- Relay coverage: every legacy relay records `legacy delivered/total · PQ delivered/total`, alive peers by kind and the
  relay
  counter (`p2p_legacy::relay_coverage`, `mempool.relay` in the metrics feed, mempool card line
  `data-testid="relay-coverage"`);
  WARN `relayed transactions reached no legacy peer` when legacy peers are alive but none took the batch.

## [0.24.0]

### Fixed ROOT CAUSE of the recurring private forks (PQ delegates forging in other delegates' slots)

- Mainnet 2026-09-20, slot 12079774: the PQ Node logged "slot belongs to nicholasflamel" while the network's block in
  that slot was forged by `titan`. The active-delegates snapshot of a round (`rd:<round>`) was written by a **2-second
  timer** that only fired when it happened to observe `tip % 21 == 0`; blocks arriving in bursts (gossip, catch-up)
  skipped
  the boundary, and `forging_order` then fell back to the **live ranking** a mid-round ordering that differs from the
  legacy snapshot (`applyRound`) whenever a vote balance moved. Two PQ delegates sharing that view accepted each other's
  wrong-slot blocks -> private branch (...754/...755) -> the network orphaned it -> stall.
    - `Storage::apply_blocks` now takes the snapshot itself, under the apply lock, right after the boundary block
      commits (bulk batches are split at round boundaries); round 1 is seeded from the genesis ranking; rolling the
      boundary block
      back deletes the snapshot of that branch.
    - `forging_order` returns `None` without a snapshot and the forger **skips the slot** with an ERROR instead of
      guessing (`forging_order_or_live` remains for display / finality bookkeeping). The round tracker only warns now.
    - Regression: `tests/round_snapshot.rs`.
- **Catch-up ping-pong**: after a fork rollback, ranges already downloaded from the peer that served the orphaned branch
  (the sibling PQ Node) were re-applied from `pending`, the fork was detected again, rolled back again forever
  (`fork detected during catch-up ... 755 -> 754` every second in the operator log). The supplier of the rolled-back tip
  is
  now parked as a block source and `fork_depth` is kept until the tip is clearly past the fork.
- Single-block applies (gossip, `postBlock`, forger, follow loop) always write an undo record, even while catch-up runs
  in bulk mode a fork rollback can no longer fail with "no undo record".
- Stall watchdog: `branch_check` compares our tip with the network branch (best legacy peers' `getBlocks(tip)` ->
  `previousBlock`) and rolls back automatically (`stall recovery: local tip ... is NOT on the network branch`), alerting
  `resync` when undo records are missing.
- `audit-all --fix` rebuilds the `dv:` vote index from the repaired wallets and prints the top-51 changes.

## [0.23.0]

### Fixed ROOT CAUSE of the wallet-state drift and of relay nodes stalling for hours (mainnet 2026-09-19)

- **Double apply race.** A block reaches a PQ Node over several intake paths at once (iroh gossip, legacy pull in the
  follow loop, legacy `postBlock`, the forger itself). Every path read the tip, then called `apply_blocks`; the sled
  transaction never re-checked that the chain still ended right before the block, and wallets are written as deltas
  (`state.apply(delta)`), so the second pass booked the block's transactions **again**: nonce +1 and balance
  − (amount + fee) per transaction exactly the `nonce +5 / −594 STH` drift found on `Sau5...`. The undo record was
  overwritten by the second pass too, so a rollback could not repair it.
    - `Storage::apply_blocks` now verifies **inside the transaction** that `meta:last_height == block.height − 1` and
      that
      the block's `previousBlock` sits at that height; otherwise it aborts with the new `Error::StaleTip` and nothing is
      written. Apply and rollback additionally share one process-wide lock.
    - `StaleTip` is *not* a peer fault: the follow loop / catch-up resync their tip silently, gossip logs at DEBUG, the
      legacy `postBlock` route answers `status: true` for a block it already has (legacy behaviour).
    - Regression tests: 4 threads applying the same block -> exactly one succeeds, wallet booked once, rollback exact
      (`concurrent_apply_of_the_same_block_is_booked_once`); forge -> orphan -> fork rollback -> network branch -> audit
      clean (`forge_orphan_rollback_keeps_wallets_consistent`).
- **Relay stall ("1h 38m ago", 667 behind, 15/16 legacy peers alive).** The same race made the follow loop report
  "peer blocks rejected" -> `record_blocks_failure` -> the peer parked as a block source for 30s x 2ⁿ (up to 10 min)
  with
  the failure counter never reset while gossip delivered 100 % of blocks. After an hour every legacy peer was parked;
  when the iroh neighbours dropped and gossip missed one block, nothing could fill the gap until a restart.
  Fixes: no penalty on `StaleTip`; the follow loop re-reads the tip after `getBlocks` and re-polls when it moved; when
  every alive peer is parked the parking is lifted at once (`PeerTable::unpark_blocks`) instead of waiting.
- Test harness bug uncovered by the guard: `tests/sobj.rs` rolled blocks back without resyncing its cached tip and the
  storage silently accepted blocks at wrong heights before harness fixed (`Chain::rollback`).

### Fixed legacy nodes did not count PQ Nodes as peers

- `p2p.peer.getStatus` reply now matches the core 3.8.2 JSON schema: `config.network.explorer`,
  `config.network.token{name,symbol}` and a non-empty `config.plugins` (`@arkecosystem/core-api` -> API port / enabled).
  protobuf drops empty strings and empty maps, and the legacy schema marks all three **required**, so PQ Nodes were
  rejected by the peer verifier. `LegacyServer::with_api(port, enabled)`; test extended in `tests/legacy_server.rs`.
- `/api/node/configuration.core.implementation` and `/api/ntfry/*` now report `sth-core-pq` (explorers can tell PQ Nodes
  from legacy nodes).

### Added stall watchdog, peer acceptance check, audit result on the metrics page

- **Stall watchdog** (`intake::spawn_stall_watchdog`, every blocktime): when the tip has not moved for 3 slots while
  legacy /
  iroh peers report a higher height -> `ERROR CHAIN STALLED: tip ... unchanged for Ns while N peer(s) report up to ... block-source
  parking lifted` (repeated every 10 slots while it lasts) and `PeerTable::unpark_blocks()` as the recovery action.
  State in
  `/api/ntfry/metrics` -> `chain.stall{stalled,height,seconds,networkHeight,peersAhead,alerts}`; metrics page shows a
  red
  `stalled · ... behind · N peers ahead` pill (`data-testid="stall-pill"`) and marks the "last block" card. Unit test.
- **`sth-core peers --accepted [--public-ip IP]`**: asks every alive legacy peer for its `getPeers` list (legacy nodes
  only
  return peers that passed their verifier) and reports which of them list this PQ Node (IP from `--public-ip` or
  `p2p.legacy_public_addr`), with a verdict (`OK` / `partially accepted` / `NOT ACCEPTED` + hints).
  `p2p_legacy::acceptance_check`.
- **Nightly audit on the metrics page**: `audit-all` stores its summary in `meta:last_audit` (`AuditSummary`: time,
  height, wallets,
  drifted count + up to 20 addresses, fixed, duration, version); `/api/ntfry/metrics.node.lastAudit`; new card "state
  audit"
  (`data-testid="audit-card"`): `ok` / `N drifted` / `never`, with age (warns when older than 2 days).

### Fixed transactions were forged only by PQ delegates (relay reached PQ Nodes only)

- `relay_fanout` defaulted to 3 peers picked by latency in practice the operator's own PQ Nodes; legacy delegates never
  had the transaction in their pool, so it waited for the next PQ delegate slot (mainnet 2026-09-19: both transaction
  blocks
  by `technolog`, 1.5–2 min after submission). Legacy core broadcasts to `maxPeersBroadcast` = 20. Now
  `p2p_legacy::relay`
  enforces `MIN_RELAY_FANOUT` = 20 (covers every alive peer on mainnet), the default is 20 and the relay log lists the
  IPs.
- Peer echoes of a transaction already in the pool (`ERR_DUPLICATE`) are logged at DEBUG (were WARN + "invalid=1").
- `repair_wallet` (audit `--fix`) now updates the `dv:` vote index of the wallet's delegate along with the balance.

### Added `sth-core doctor`, stall auto-recovery, operator alerts

- **`sth-core doctor [--public-ip IP] [--api URL]`**: one verdict from `/api/ntfry/metrics` (node, sync/behind, stall,
  last block
  age, legacy / iroh peers, forging, last audit age & drift, mempool) + the legacy acceptance check;
  `[ OK ] / [WARN] / [FAIL]`
  lines, exit code 0 / 1 / 2 (cron-friendly). `doctor::evaluate` is pure and unit-tested.
- **Stall auto-recovery**: one slot after the CHAIN STALLED alert (parking lifted) the watchdog pulls the gap directly
  from up
  to 3 iroh peers that are ahead (`p2p_iroh::fill_gap_from`, shared with the gossip gap-fill); "chain recovered" is
  logged.
- **Alerts** (`alerts:` in node.yaml `telegram_bot_token` + `telegram_chat_id` and/or `webhook_url`, `cooldown_secs`
  600):
  CHAIN STALLED / recovered, fork rollback in the follow loop, `audit-all` drift (CLI, awaited). Telegram `sendMessage`
  or
  `POST {kind,text,at}`; messages tagged `[sth-core-pq <legacy_public_addr|network>]`; per-kind cooldown, recovery
  bypasses it.
  `tests/alerts.rs`.

### Added `sth-core audit-all [--fix] [--json]`

- One pass over the whole chain reconstructs nonce / balance of **every** wallet (transfers, multipayments, HTLC lock /
  claim / refund via the lock transaction, forged fees + rewards, TokenInit burns) and compares with the stored state.
  Exit code 2 when drift is found; `--fix` repairs the listed wallets (node stopped); `--json` for cron. Wallets touched
  by
  sObject / token-market purchases are reported as `skipped` (counter-party only known from state).
  `WalletAudit.forged`.
- `Storage::audit_all` / `ChainAudit`; `audit_wallet` shares the same bookkeeping and now credits HTLC claim / refund.

## [0.22.0]

### Fixed forged block orphaned by the network left the node on a private fork

- Incident 2026-09-19 (mainnet 11883552 / 11883719): the node forged a block with a transaction the legacy network
  rejected (`ERR_BAD_NONCE` on their side), the slot was skipped and the node kept its own block for 10+ minutes
  (`0 of 13 peers agree`).
- **Orphan watchdog** (`Forger::spawn_orphan_watchdog`): two slots after forging, the healthy legacy peers are asked
  which block
  they have at our height; when most serve another id the block is rolled back at once (legacy `forkBlock` behaviour)
  and the
  follow loop resyncs no manual `sth-core rollback` needed.
- Forged blocks now go to **every** healthy legacy peer (not `broadcast_fanout`); a peer's `postBlock` rejection is
  logged at WARN
  with the peer, "none accepted" at ERROR.
- **Wallet state audit**: `GET /api/wallets/:id/audit` and `sth-core audit-wallet <address> [--fix]` recompute nonce /
  balance from
  the wallet's transaction index and compare with the stored state (`nonceOk`, `balanceOk`, `sent`, `received`,
  `skipped`);
  `--fix` (node stopped) overwrites drifted state. Root cause of the incident: a wallet stored with nonce 348 / balance
  −594 STH
  while its history (and the network) said 343 every transaction built from that node's nonce was invalid for the
  network.
  `tests/wallet_audit.rs`.

### Fixed stuck pending transactions blocked the sender's nonce until a restart

- Pending transactions the network never forged stayed in the pool forever, so `POST /api/transactions` demanded
  `chain nonce + pending + 1` while every other node still expected `chain nonce + 1` (`ERR_BAD_NONCE ... nonce 1254, expected
  1256`). Now, as in the legacy `transactionPool.maxTransactionAge`, each pending transaction remembers its entry height
  and is
  dropped after `mempool.max_age_blocks` (default 2700 ≈ 6 h, 0 = never) or once its own `expiration` height is reached
  WARN
  `Dropped pending transaction never forged` with age; counter `Mempool::expired()`.
- Pending transactions are re-relayed every `mempool.rebroadcast_blocks` (default 30 ≈ 4 min) over legacy P2P and iroh
  gossip (`Mempool::subscribe_rebroadcast`, published without the gossip seen-filter) so a failed first relay no longer
  strands them.
- RBF-lite: a transaction whose nonce is still pending here from the same sender **replaces** the pending one when its
  fee is
  not lower (INFO `pending transaction replaced by the sender`); a lower fee is refused with the pending id and required
  fee.
  Pool policy only consensus is untouched.
- `ERR_BAD_NONCE` explains itself: `nonce 1254, expected 1256 (chain nonce 1251 + 4 pending in this node's pool: 1252–1255;
  resend a pending nonce with an equal or higher fee to replace it, see GET /api/transactions/unconfirmed?senderId=...)`.
- `GET /api/transactions/unconfirmed?senderId=<address>` filter, items sorted by sender + nonce, each with
  `pool { enteredHeight, ageBlocks, lastRelayHeight }`; `DELETE /api/transactions/unconfirmed[?senderId=]` purges the
  pool
  without a restart accepted only from a direct 127.0.0.1 connection without reverse-proxy headers (`X-Forwarded-For`,
  `X-Real-IP`, `Forwarded`), never over iroh. `sth-cli mempool [--sender]`, `sth-cli mempool-purge [--sender]`.

### Added SHIP-44 Wallet Stream API (wallets stop polling REST)

- `netfory/api/1` (+ legacy `netfory/api/0`, alias `sth/api/1`) on the node's iroh endpoint: a netfory-provider
  `MeshPacket`
  (`method`, `path`, base64 `body`, `headers`) is executed by the axum REST router **in-process** and answered with the
  node
  JSON plus the `pdata` envelope (`node_id`, `status`, `signed_at`, `sig`, `body_b64`) signed by the node key the
  EndpointId is the verification key, so `api://<EndpointId>/...` clients work against `sth-core` without a provider
  process.
  Every `/api/*` endpoint incl. `POST /api/transactions`; errors keep the legacy envelope (503 disabled, 429 rate
  limit).
- `sth/events/1` push sessions: `subscribe { addresses, topics: [blocks|wallet|mempool|dex], tokens, sinceHeight }` ->
  `hello`, `subscribed`, `block`, `tx`, `balance`, `mempoolTx`, `order`, `trade`, `rollback`, `resync`, `pong`, `error`
  frames (`[u32 LE len][JSON]`, REST JSON shapes). Reconnecting wallets get missed confirmed transactions replayed from
  the
  `wt:` index (≤ 200, else `resync`) and fresh balances.
- `Storage::subscribe_chain()` -> `ChainEvent::{Applied(Arc<Block>), RolledBack{height}}` broadcast after every apply /
  rollback (sent only while someone listens); `storage::tx_addresses` is public.
- node.yaml `p2p.iroh.wallet_api { enabled: true, max_connections: 2000, max_addresses: 64, rate_limit_rps: 20,
  idle_timeout_secs: 300 }` defaults for a 1–2 GB node (idle QUIC connection ≈ 60–100 KB); per-EndpointId token bucket,
  connection cap (close code 1), frame caps, idle timeout. `/api/node/status -> data.irohWalletApi` stats.
- Reference client `p2p_iroh::wallet_api::client` (`call`, `call_on`, `verify_pdata`, `EventStream`), tests
  `tests/iroh_wallet_api.rs`, spec `https://github.com/smartholdem/SHIPs/blob/master/SHIPS/SHIP-44.md`, wallet developer
  guide `docs/WALLET-IROH-INTEGRATION.md`.
- `sth-core iroh-id --url [--name <provider>]` prints the node's provider-style address `api://<EndpointId>/sth-core`;
  the same
  URL is in `/api/node/status -> irohWalletApi.url` and the startup log. Request paths without `/api` are mapped onto
  `/api/...`
  so `api://<id>/api/blocks` works whichever way a client splits it.
- MessagePack event frames: `hello.encodings = ["json","msgpack"]`; `{ "type": "encoding", "encoding": "msgpack" }` (or
  the key
  on any command) switches the session, commands are auto-detected by codec (`rmpv` with-serde). Reference client
  `EventStream::set_encoding`, `last_encoding`.
- WebSocket bridge for browser wallets, `p2p.iroh.ws_bridge { enabled, listen: 0.0.0.0:4005, default_node }`:
  `ws://.../ws/events[/<EndpointId>]` (one WS message = one `sth/events/1` frame) and `ws://.../ws/api[/<EndpointId>]`
  (one WS
  message = one MeshPacket, reply = signed response), `GET /` info. Dials targets from a private iroh endpoint (iroh
  refuses
  self-connections), so any node can front itself or another node. `p2p_iroh::ws_bridge::{serve, bind}`; axum `ws`
  feature.
- Docs: `docs/WALLET-WS-MSGPACK.md` (browser WebSocket bridge + MessagePack for wallet developers and node
  operators),
  nginx `wss://` templates `docs/nginx/ws-bridge.smartholdem.io.conf` (own domain) and
  `docs/nginx/snippets/sth-ws-bridge.conf`
  (same domain as the REST API).

## [0.20.0]

### Changed API caches keyed by a storage write counter

- `Storage::generation()` monotonic counter bumped after every block apply / rollback / wallet write. `/api/wallets`,
  `/api/wallets/top`, `/api/delegates/:id/voters` share one balance-sorted wallet snapshot and `/api/delegates*` its
  ranking,
  both rebuilt only on the first request after a chain write (before: a full sled wallet scan + sort + JSON of every
  wallet on
  each call; delegates were keyed by height, which missed a rollback + re-apply at the same height). Pages are sliced
  before
  rendering JSON.

### Added

- `sth-core iroh-id` prints the bare EndpointId (banner and logs go to stderr) so `$(sth-core iroh-id)` works in
  scripts;
  `sth-cli tx sn-register` / `sn-update` take `--node-id` optionally and otherwise derive it from `--key-file`,
  `node.yaml`
  (`--node-config`, `p2p.iroh.secret_key_file`), `./iroh.key` or the running node (`/api/node/status.irohEndpointId`,
  new
  field). The SN public REST URL flag is `--url` (`--api` is the CLI's node endpoint).
- SHIP-42 stage 1 Service Node registry: sObject `type 7` with
  `ntfryData = iroh://<EndpointId>?roles=...[&api][&storage]`,
  milestone `masternodes { enabled, collateralAmount }`, rules (data format, one per wallet, collateral floor at
  registration, not transferable / sellable), `GET /api/masternodes[?active&role]`, `GET /api/masternodes/:id`,
  `serviceNodes` in `/api/ntfry/metrics`, `constants.masternodes` / `constants.tokenMarket` in
  `/api/node/configuration`,
  `sth-cli tx sn-register | sn-update | sn-resign`, `sth-cli masternodes`. Collateral is a live balance floor (DASH
  model).
- `GET /api/tokens/:key/trades` executed SHIP-43 fills newest first (buyer, seller, amount, price, total, height, time)
  from
  the `tt:<token>:<height>:<seq>` index written with the block and removed on rollback; `?groupBy=day|hour&days=N`
  returns
  OHLCV candles for price charts.
- SHIP-43 stage B the token market itself. `TokenSell` moves the units into escrow inside a never-deleted order record
  (`to:` / book `tob:<token>:<price>` / seller `tos:`), `TokenBuy` fills part or all of it in one atomic state change
  (STH
  buyer -> seller, units escrow -> buyer, strict balance check independent of `strictBalance`), `TokenCancel` returns
  the
  remainder; several fills of one order in a block are applied in order and an over-fill rejects the block. Rollback is
  exact (un-fill / reopen / drop). Mempool and batch views overlay pending orders. API: `GET /api/tokens/:key/orders`,
  `/orders/:orderId`, `/api/wallets/:address/orders`. CLI: `sth-cli tx token-sell --ticker --amount --price`, `token-buy
  --order [--amount]`, `token-cancel --order`, `sth-cli token-orders TICKER`.
- SHIP-43 stage A (token market wire format): `TokenSell = 5` (`amount ‖ price`), `TokenBuy = 6` (`orderId ‖ amount`),
  `TokenCancel = 7` (`orderId`) in typeGroup 3, `TokenAsset { price, orderId }`, milestone `tokenMarket { enabled,
  maxOpenOrders }`, `tokenFees.sell/buy/cancel` (1 / 0.1 / 0.1 STH). Rejected with `TokenTypeNotActiveError` until the
  milestone and with `TokenMarketNotImplementedError` by the wallet-aware check until stage B (order book state
  machine);
  state application explicitly ignores the three types so they can never be misread as burns.
  `https://github.com/smartholdem/SHIPs/blob/master/SHIPS/SHIP-43.md`.
- `GET /api/wallets/search?q=<prefix>&limit=10` explorer search box over the cached wallet snapshot: address prefix,
  delegate username prefix (case-insensitive) or public key prefix, richest first, with `match` kind per row.
- `https://github.com/smartholdem/SHIPs/blob/master/SHIPS/SHIP-42.md` Service Nodes (Masternodes) Protocol & Netfory
  Infrastructure Incentives (Draft).
- `GET /api/node/fees?days=N&groupBy=day` per-UTC-day activity buckets (`count`, `sum`, `blocks`, per-type
  `avg/max/min/sum`) for
  explorer charts, 1–90 days in one index pass; closed days are cached in memory so repeat calls rescan only today. The
  legacy
  `/api/node/fees?days=N` shape now walks the transaction index too instead of decoding every (mostly empty) block of
  the window.

### Fixed

- Legacy nodes accepted a gateway (`legacy_listen`) Rust node only by luck. Core 3.x pings a new peer back and runs
  `PeerVerifier.checkState`: it rebuilds the block from `getStatus().state.header` and checks its signature, then (when
  heights
  differ) searches the fork point with `p2p.peer.getCommonBlocks`. We answered with a 2-field header (id, height) and
  had no
  `getCommonBlocks` route, so verification failed unless our tip happened to equal the peer's last block. `getStatus`
  now returns
  the full header (`idHex`/`previousBlockHex` included, `config.plugins` map) and `p2p.peer.getCommonBlocks` returns the
  highest
  asked id present in our chain legacy `/api/peers` lists the Rust node, and it counts toward the legacy quorum.
- `/api/blockchain` `supply` and delegate `production.approval` were the mainnet constant (249 770 000 STH) on every
  network.
  Now `Network::supply(height)` = genesis `totalAmount` + block reward of each height per milestone (legacy supply
  calculator), so
  testnets / `newnet` chains with their own genesis and `reward` milestones report the real emission.
- `/api/transactions` (and `/api/votes`, wallet transaction lists) no longer scans the whole chain per request: the
  listing walks
  the height-ordered sled index lazily (`skip/take` straight on the index, one block decode per row) and `totalCount`
  comes from a
  persistent counter `meta:tx_count` kept in the same sled transaction as the blocks (built once on first start of an
  existing
  database). Mainnet `GET /api/transactions?limit=10` went from 30–45 s to milliseconds. `timestamp[.from/.to]` filters
  map to a
  height window by binary search; filters that need the decoded transaction (`type`, `senderId`, `vendorField`, ...)
  decode each
  block once and stop counting after 50 000 index entries the rest is extrapolated and reported as
  `totalCountIsEstimate: true`
  (legacy PostgreSQL semantics). `api.cors_origins` (default off) adds CORS headers for browser clients on both API
  ports.
- Sync log printed every legacy peer as `ip:4001` regardless of the network's `p2p.legacy_port` (newnet = 4002);
  connections
  always used the configured port only the label was wrong. Peers are now shown as `legacy:<ip>`.
- `StaticFeeMismatchError` for token transactions now reports the fee the transaction pays next to the expected one, so
  a relay
  running other `tokenFees` / core version than the forger is diagnosable from the log (`docs/NEWNET.md` -> "Второй
  узел").

### Added SHIP-35 BFT finality gadget (soft mode by default, legacy-safe)

- Third iroh gossip topic `finality` (`sha256("sth/<nethash>/finality")`), `GossipMessage::Finality { votes }`,
  `FinalityVote { height, blockId, publicKey, signature }` = Schnorr over
  `sha256("sth-finality-v1" || height LE || blockId)`.
- `p2p_iroh::finality::FinalityTracker`: verifies votes (signature + membership in the active set of the round, height
  window
  `[tip-120, tip+21]`), stores a certificate `fc:<height>` once ≥ quorum (`Milestone::finality_quorum()` = ⌊2n/3⌋+1 = 15
  of 21,
  override `finality.quorum`) votes name the block that is in our chain; equivocation guard for our own keys.
- Every delegate key in `delegate.secrets` votes for each new tip (any source) via `publish_finality`, re-sends once a
  slot later
  while uncertified; `set_forging_keys(keys, announce)` votes no longer depend on `delegate.announce`.
- Rollback guard in `Storage::rollback_last_block`: soft mode (default) warns and drops the orphaned certificate; hard
  mode (milestone `finality.active: true`) refuses with `FinalityViolation`. Blocks, transactions and the legacy
  protocol are unchanged.
- API `GET /api/ntfry/finality` (also on the metrics port), `metrics.finality`, `configuration.finality`; metrics page
  card
  "finality" (`data-testid="finality-card"`). `sth-core init newnet --finality-hard`. Tests: `tests/finality.rs`.
- iroh RPC `GetFinality` -> `Response::Finality { certificate }` (`fetch_finality`): a node whose certificate lags the
  tip by > 2
  blocks asks up to 3 peers (10 s after start, then every minute) and adopts the best certificate only after
  `FinalityTracker::adopt_certificate` re-verifies quorum, active-set membership and every signature against a block in
  its own chain.
- Block JSON (`/api/blocks`, `/api/blocks/:idOrHeight`, `/api/blocks/last`, delegate blocks, metrics) ->
  `finalized: bool`
  (height ≤ highest certificate); transaction JSON -> `finalized: bool` too. `sth-cli status` shows a "finality" row,
  `sth-cli tx-status` / `watch` print "final" instead of the confirmation count once the block is certified.
- Slashing (SHIP-35): `FinalityTracker::record` detects a key voting for two ids at one height, stores
  `EquivocationProof { height, publicKey, blockIds[2], signatures[2], detectedHeight, bannedUntilRound }` (`eq:<pk>`)
  and gossips
  `GossipMessage::Equivocation`; receivers re-verify (`verify_proof`: both signatures + ranked active set of the round),
  clamp the
  ban to their own round + `slashingRounds`, relay once. Milestone `finality: { slashing: true, slashingRounds: 30 }`
  (default
  off, Rust-only) makes `Storage::active_delegates` hence round snapshots and forging order skip the delegate until
  `bannedUntilRound`. `/api/ntfry/finality` -> `slashing`, `equivocations[]`; metrics card shows "⚠ N double votes".
  `init newnet --finality-hard` now also enables slashing.
- `docs/MILESTONES.md`: how to switch every feature on through `milestones.json` (keys, examples, mainnet order).
- Equivocation dashboard: proofs are kept as history (`eq:<pk>:<height>`, `Storage::equivocation_history`),
  `/api/ntfry/delegates`
  -> per delegate `doubleVotes`, `bannedUntilRound`, `banned` + `slashing { enabled, rounds, currentRound }` +
  `equivocations[]`;
  metrics page: column "double votes / banned until" in the delegates table and an "equivocation proofs" table
  (`data-testid="equivocations-table"`). `cargo run --example seed_equivocation -- <net dir>` seeds a demo proof.
- `sth-cli tx`: core v2 transactions (transfer) pay the node's SHIP-41 dynamic minimum
  `(addonBytes + wire bytes) × rate`
  from `configuration.transactionPool.dynamicFees` (or `node.yaml` when run next to the node), rounded up to 0.001 STH
  (`cli::dynamic_fee`); the `fee` line prints the calculation. `--fee <STH>` overrides it (core types only); static fee
  is used
  when the node has dynamic fees disabled. Transfer now costs 0.008 STH instead of 1 STH.
- `sth-cli tx vote --delegate <username|publicKey>` / `sth-cli tx unvote`: type-3 vote transactions with the same
  SHIP-41
  dynamic fee (158 B -> 0.008 STH); voting while voting for someone else emits `["-old", "+new"]` in one transaction,
  voting
  for the current delegate / unvoting without a vote / unknown delegate are clear errors.
- `sth-cli tx delegate-register --username <name>` (validates the legacy username charset/length, refuses a taken name
  or a
  wallet that is already a delegate, prints the node.yaml follow-up) and `sth-cli tx delegate-resign` (refuses
  non-delegates and
  repeated resignations). Both pay the SHIP-41 dynamic fee (registration ≈ 12.005 STH on mainnet constants, resignation
  0.007 STH);
  `sth-cli wallet` marks resigned delegates.
- Milestone keys renamed to SHIP names: `ship11` (was `aip11`), `ship11Blocks` (was `aip37`), `ship13` (was `aip36`) in
  the
  embedded `network/mainnet/milestones.json`, `init --network-files` export and `init newnet`. Historical `aip*` /
  `txV2` /
  `schnorrBlocks` / `sobj` remain accepted as aliases (`config::MILESTONE_KEY_ALIASES`, canonicalised before the
  deep-merge so
  mixed spellings across milestones do not collide). `configuration.constants` reports both spellings.
- `sth-cli delegate-setup --username <name> [--passphrase] [--fund-from <passphrase>] [--reserve 1] [--no-pq] [--pq-passphrase]
  [--node-config node.yaml] [--timeout 60]`: one-pass delegate wizard wallet (fresh mnemonic printed once), funding
  (fees + reserve
  computed, transferred from the funding wallet, confirmation awaited), `delegate-register`, `pq-register` when
  `pq.active`, and a
  comment-preserving text patch of `node.yaml` (`delegate.enabled`, `secrets`, `pq_secrets`; `cli::patch_node_yaml`).
  Rerunning skips
  finished steps and never duplicates entries.
- Type 5 is documented as the Netfory content pointer everywhere (SHIP-41 table + note);
  `mempool.dynamic_fees.addon_bytes.ntfry` is accepted as an alias and `/api/transactions/types`
  reports  `Ntfry: 5`.
- SHIP-41 "Dynamic Fees Per-Node Pool Policy and the Wallet Fee Formula" (Active): formula, mainnet constants, wire
  sizes (transfer 156 B, vote 158 B, delegateRegistration 124 + username, multipayment 125 + 29·N), API surface for
  wallets.

### Added Quantum Shield stage C: hybrid block signatures (SHIP-22, milestone `pq.blocks`, default off)

- Block version 1: `pqSignature = alg u8 || len u16 LE || ML-DSA-44(M_B)`, `M_B = sha256(header || blockSignature)`,
  ML-DSA
  context `sth-pq-block-v1`, id = sha256 over both signatures. JSON `pqSignature: { algorithm, signature }`; storage /
  snapshot /
  legacy `postBlock` framing reuse the same serializer (`serialize_block`, `block_pq_message`,
  `verify_block_pq_signature`).
- Milestone `pq: { blocks, blocksGrace (43 200) }`, `Network::pq_blocks_activation_height / block_versions_allowed`: v1
  only from
  activation, v0 only until the grace window closes (`BlockPqRequiredError`); `verify_block` checks version /
  algorithm / length,
  `check_stateful_rules` verifies `pqSignature` against the delegate's `pq_key` as of the state before the block
  (`BlockPqKeyMissingError`, `BlockPqSignatureError`); a rotation inside block H is valid from H+1; undo through wallet
  state.
- Forger: `delegate.pq_secrets` / `STH_DELEGATE_PQ_PASSPHRASE` (ML-DSA passphrases matched to delegates by their
  on-chain key),
  `forge_block_with(..., pq)`; without a key: v0 + warning inside the grace window, slot skipped afterwards. `DelegateStatus.pqRegistered /
  pqSecret`, `/api/ntfry/delegates` -> `pqKeys`, `pqBlocks { active, activation, graceEnd, blocksLeft }`, per-delegate
  `pqKey`;
  Delegate Dashboard shows `pq ✓ / pq missing` and the grace countdown. `sth-core init newnet --pq-blocks-at H`
  (21-block grace).
  `configuration.pq.blocks*`, `quantumShield.stage: "C"`, block JSON `pqSignature { algorithm, bytes }`. Tests:
  `tests/pq_blocks.rs`.

### Added fee metrics

- `Mempool::low_fee_rejected()` (count of `ERR_LOW_FEE`), `Mempool::min_transfer_fee()` (dynamic
  `(addon + 156 B) × rate` or static);
  `metrics.mempool.dynamicFees { enabled, minFeePool, minFeeBroadcast, addonBytes, minTransferFee, transferBytes, lowFeeRejected }`.
  Metrics page: mempool card shows `min transfer 0.0077 STH (dynamic) · N low-fee rejected`; the payout calculator uses
  the dynamic
  minimum for transfers and multipayments when dynamic fees are on.

### Added dynamic fees (per-node pool policy, legacy `transactionPool.dynamicFees`)

- `node.yaml` ->
  `mempool.dynamic_fees { enabled (true), min_fee_pool (3000), min_fee_broadcast (3000), addon_bytes {...} }` with the
  mainnet legacy values (`transfer` 100, `vote` 100, `delegateRegistration` 400 000, ...). Partial overrides keep the
  other defaults.
- Mempool (`Mempool::check_core_fee`): a core v2 transaction enters the pool at
  `fee ≥ (addon_bytes[type] + wire bytes) × min_fee_pool`
  (≈ 0.0076 STH for a transfer instead of the 1 STH static fee) and is relayed / gossiped only from `min_fee_broadcast`
  (below it:
  accepted, kept local `accept` without `broadcast`, as in legacy). `enabled: false` demands exactly the static fee of
  the
  milestone. Both cases answer `ERR_LOW_FEE`. Before this the pool did not check core fees at all (fee 1 was accepted).
  v3 (PQ) keeps
  its own `static + surcharge` rule; sObject / token fees stay exact.
- Not a consensus rule: blocks with cheaper transactions from other nodes are applied as before
  (`tests/dynamic_fees.rs`).
- `GET /api/node/configuration` -> `transactionPool.dynamicFees` reports the real policy (`enabled, minFeePool, minFeeBroadcast,
  addonBytes`) so wallets compute the same fee as against a legacy node instead of falling back to the static 1 STH.

## [0.19.0]

### Added SHIPs (SmartHoldem Improvement Proposals)

- `https://github.com/smartholdem/ships`: index `README.md` + 40 proposals in the standard SHIP format (preamble, Abstract, Motivation,
  Specification,
  Rationale, Compatibility, Reference Implementation). Active/Final: Rust core, Sled storage, Iroh gossip/RPC,
  dual-stack
  networking, snapshots, parallel verification + vote index, metrics/Version Guard, `init newnet`, `sth-cli`, wire
  format v2,
  sObjects, Quantum Shield A. Accepted: native tokens, TokenMeta, sObject market, STH burn, wire v3, Quantum Shield B.
  Draft: multipayment 1024, wallet shield UX, hybrid block signatures, governance (11-of-21, 2-week window),
  TokenUpdate,
  TokenFreeze, NFT, HTLC token swaps, deflationary tokens, confidential transfers (STARK pool), genetic chain
  compression,
  pruning, lane sharding, Safe Contracts, Dead Crypto, BFT finality, compact blocks, Netfory Web4, light clients,
  sub-second
  slots, ML-KEM encrypted memos.
- Terminology: all references to the predecessor project and its proposal numbers removed from code comments and docs;
  milestone struct fields renamed `tx_v2` / `sobj_active` / `schnorr_blocks` (historical JSON key names kept for wallet
  compatibility, aliases `txV2` / `sobj` / `schnorrBlocks`), `TYPE_GROUP_SOBJ`; 

### Added Quantum Shield stage B (v3 transactions, ML-DSA-44) behind milestone `pq`

- Wire format v3 (`SPEC-PQ-V3.md`): type-1 payload `alg || pk_len || pk`, second-signature section as
  `alg || len || sig` blocks,
  JSON `secondSignatures: [{ algorithm, signature }]`, `asset.signature.algorithm`; `M2 = sha256(BODY || SIG1)`, ML-DSA
  context
  `"sth-pq-v1"` (spec aligned with the code and the published vectors).
- Milestone `pq: { active, feePerByte (10 000), commitmentGrace (86 400) }`; `Network::pq_activation_height()`;
  `sth-core init newnet --pq-at H`.
- Rules (`rules::check_pq_format` / `check_pq`): activation gate, algorithms / sizes / order, fee surcharge (core ≥
  static + surcharge,
  sObject / token exact + surcharge), registration with proof of the current key (legacy alg 0 or old PQ key), rotation,
  commitment
  grace window, PQ-locked wallets must send v3 with exactly one block; batch-aware in `check_stateful_rules`,
  pending-registration-aware
  in the mempool (`ERR_PQ_*` codes). `WalletState.pq_key { algorithm, publicKey, since }` replaces `secondPublicKey` on
  registration.
- API: `configuration.pq`, `quantumShield.stage`, wallets `quantumShield.{active, algorithm, publicKey, since}`,
  transactions `secondSignatures`.
- CLI: `sth-cli tx pq-register [--old-second-passphrase]`, `--second-passphrase` / `STH_SECOND_PASSPHRASE` for every
  `tx ...` (legacy v2
  second signature or automatic v3 + surcharge for PQ-locked wallets); `sth-cli wallet` shows the shield state.
- Tests `tests/pq_v3.rs`: wire round-trip, activation, registration / lock / rotation / rollback, legacy migration +
  commitment window, mempool codes.

### Added operations

- Mempool **byte budget** `mempool.max_bytes` (default 8 MB, `Mempool::with_max_bytes`): `ERR_POOL_FULL` once the wire
  bytes of pending
  transactions would exceed it; `/api/ntfry/metrics` -> `mempool.bytes / maxBytes`, `/api/node/configuration` ->
  `maxBytesInPool`.
- Metrics page: `quantum shield stage A|B: N committed · M PQ-active · pool KB` (`pqk:` index, `pq_key_count()`,
  `node.pqStage/pqActive`).
- `tests/vectors/pq_v3.json`: `v3` format notes + `transactions` (5 byte-exact v3 vectors: registration fresh / with
  legacy proof,
  PQ-locked transfer, rotation, v3 with alg-0 block) verified by `published_v3_transaction_vectors_match`.
- §C.1–C.6 stage C design draft (hybrid secp256k1 + ML-DSA-44 block signatures, delegate PQ key, `pq.blocks` milestone, verification
  order).

- `sth-cli pq-commitment` (stage A helper: prints the `sthpq1:` commitment + ready `tx transfer --memo` line) 
  delegate readiness checks, activation window, readiness table, announcement template, rollback matrix,
  post-activation).

### Added sObject safety

- **Resign guard**: `obj-resign` of a type-5 registry whose token has live supply is refused
  (`TokenSmartObjectStillActiveError`) in blocks
  and in the pool; burn the supply to 0 first.
- **Pool spend fix**: the mempool balance check counts the price of a pending sObject buy for later transactions of the
  same sender.

## [0.18.0]

### Changed legacy object terminology -> SmartObject (sObject)

- The programmable on-chain object of `typeGroup 2 / type 6` is now a **SmartObject (sObject)** everywhere the operator
  can
  see it: Rust types (`SmartObject`, `SmartObjectAsset`, `SmartObjectData`, `models::sobj`), storage API (`get_sobject`,
  `all_sobjects`, `sobj_by_name`), rules (`check_sobj*`), error names (`SmartObjectNotRegisteredError`,
  `SmartObjectNotForSaleError`, `TokenNotSmartObjectOwnerError`, ...), log lines and the metrics page.
- REST: the old legacy-named routes **removed**, replaced by `GET /api/sobj`, `GET /api/sobj/:id`,
  `POST /api/sobj/search`
  (same JSON shape). Wallet attribute renamed to `attributes.sobjects`; `/api/tokens` field renamed to `sobj`;
  `/api/transactions/fees` group 2 keys `sobjRegistration / sobjUpdate / sobjResignation`; `/api/node/configuration` ->
  `constants.sobjV2`, `tokenTickerSobjType`; transaction type name of group 2 is `SmartObject`.
- Milestone flag renamed to **`sobjV2`** (the legacy-named flag is still read as an alias). `sth-core init newnet`
  writes `sobjV2: true`.
- CLI: `sth-cli obj <id|name> [--type N]`, `sth-cli tx obj-register / obj-update / obj-transfer / obj-sell / obj-buy`
  (same for `sth-core tx`). `watch` prints "sObject registered / handed over / bought".
- **Wire format is untouched**: typeGroup 2 / type 6 bytes, `asset.{type,subType,action,registrationId,data}` JSON,
  transaction ids
  and block ids are byte-for-byte the same as before; Sled prefixes `en:` / `eo:` / `mk:` are kept (commented as sObject
  keys), so
  existing databases need no migration.
- TokenInit fee default **100 -> 500 STH** (`tokenFees.init = 50000000000`), half of it still burned
  (`INIT_BURN_SHARE_PERCENT = 50`).
  Test networks created with older `milestones.json` keep their own value.

### Added

- Milestone parameter **`tokenFees.initBurnPercent`** (0–100, default 50) replaces the hard-coded 50 % TokenInit burn
  share:
  `{ "height": H, "tokenFees": { "initBurnPercent": 0 } }` switches the burn off (or changes the share) without a code
  fork.
  Every `tokenFees` field now has a default so a later milestone may patch a single key; values > 100 are rejected at
  load.
  `/api/node/configuration` exposes `tokenFees.initBurnPercent` and `initBurn` (smartoshi per TokenInit);
  `sth-cli status`
  prints `token init 500 STH · burn 50% (250 STH)`. Test: two milestones (50 % -> 0 %) in one chain, rollback across the
  switch.
- full specification of SmartObjects: types, lifecycle (register / update / resign /
  transfer),
  market (sell / buy), `ntfryData`, fees and burn, API, CLI, JSON examples, limits, error catalogue.

## [0.17.0]

### Added

- `sth-cli create-wallet [--passphrase]`: BIP-39 12-word mnemonic from the OS random generator (`crypto::mnemonic`,
  official English
  wordlist embedded, checksum verified against the BIP-39 vectors), address with the connected network's byte, public
  key. Offline —
  nothing is sent to the node.
- `sth-cli watch <address> [--every N] [--tail N]`: live wallet feed coins in / out with memo, multipayments, votes,
  HTLC, token
  transfers / mint / burn / manifest, SmartObject registration / order / sale / purchase / hand-over; `pool` vs
  `✓confirmations`,
  `--json` streams one transaction per line.
- Wallet history now includes token recipients (transfer items, mint) and the new owner of a transferred / bought
  SmartObject (`tx_addresses` index + `/api/transactions?recipientId=` filter) previously only STH recipients were
  indexed.

## [0.16.1]

### Added

- Release pipeline: `.github/workflows/release.yml` builds `sth-core` + `sth-cli` on tag `v*` for linux x86_64 / aarch64
  and macOS
  arm64 / x86_64, smoke-tests `--version`, packs tar.gz + `SHA256SUMS` and attaches them to the GitHub release;
  `.github/workflows/ci.yml` builds and runs the test suites on every push (fmt / clippy advisory). `scripts/release.sh`
  produces the  same archive locally. 

## [0.16.0]

### Added `sth-cli`, a node client binary (bitcoin-cli style)

- Second binary in the crate, HTTP only (no Sled / P2P): `status`, `wallet`, `token`, `SmartObject`, `market`,
  `tx-status --wait`, and
  every signed transaction as `sth-cli tx ...` (transfer, obj-register / update / transfer / sell / buy, token-init /
  transfer / mint /
  burn / meta). Chain parameters (address byte, fees, blocktime) come from `GET /api/node/configuration`, so it works
  against any
  node mainnet or a private network with `--api URL` (remembered in `~/.config/sth-cli/api`) or `STH_API`. `--json`
  everywhere.
- Signer moved into the library (`sth_core::cli`, `ChainSource::{Config, Api}`); `sth-core tx ...` keeps working on the
  node itself.
- Nonce race handled: when earlier transactions of the sender are still in the pool the node's `expected N` is used and
  the
  transaction is re-signed and re-sent once. `tx-status --wait` waits until the transaction leaves the pool.
- `docs/CLI.md`.

## [0.15.0]

### Added

- **Market tab** on the metrics page (node | tokens | market): every open sale order SmartObject name, type badge,
  price, owner,
  pointer, id, token logo / supply for tickers 100 per page with prev / next. Feed
  `GET /api/ntfry/market?page=&limit=&type=`
  (`meta.totalCount / pageCount`). Backed by a new Sled index `mk:<id>` -> owner, maintained on sell / cancel / buy /
  transfer /
  resign and restored on rollback, so listing thousands of orders is a prefix scan, not a wallet sweep.
- **Version guard** on the Delegate Dashboard: milestone `minCoreVersion` (e.g. "0.14.0"; `init newnet` writes the
  generator's
  version). Rust delegates announcing an older version are marked `⚠ update to ≥ X` in red, the bar shows `N below vX`;
  `/api/ntfry/delegates` returns `outdated` per delegate plus `outdated` / `minCoreVersion` totals. Exposed in
  `/api/node/configuration` constants together with `strictBalance`.

## [0.14.0]

### Added sender balance enforced in block validation (milestone `strictBalance`)

- `check_stateful_rules` now tracks every STH movement inside the validated batch sender debit `amount + fee (+ multipayment
  sum)`, transfer / multipayment credits, HTLC claim / refund credits (locks opened in the batch included), SmartObject
  purchase price,
  forger reward + fees **after** the block's transactions (legacy order, minus the TokenInit burn) and rejects a block
  whose sender
  cannot cover the spend: `InsufficientBalanceError: sender ... has X, needs Y`, exactly like legacy
  `throwIfCannotBeApplied`.
  Coins received earlier in the same block count; coins arriving later or the forger's own reward of that block do not.
  Height 1 is
  exempt (genesis distributes from nothing).
- Milestone flag `strictBalance` (default **false**, `init newnet` -> true). With the flag off the node applies the
  block and logs
  `WARN balance check would fail (milestone strictBalance is off)` mainnet operators can resync history and confirm
  there are no
  hits before the flag is switched on in the same milestone as `tokens` / `sobjV2`.
- Mempool: an SmartObject purchase is checked against the balance **minus** the sender's pending pool spend.
- Tests `tests/balance.rs`.

## [0.13.1]

### Fixed

- **Catch-up stalled on a block with an SmartObject purchase** (`SmartObjectInsufficientBalanceError` -> "peer blocks
  rejected, re-requesting
  range" forever): batch validation checked the buyer's balance against the pre-batch wallet, ignoring coins received
  earlier in the
  same 400-block range. `check_stateful_rules` now tracks STH moved inside the batch (transfers, multipayments, purchase
  prices) and
  validates the purchase against the effective balance. Blocks applied one by one (live follow) were never affected.
- A private network's first node now learns about nodes that dial in: an inbound legacy connection that speaks
  `p2p.blocks.*` is
  added to the peer table (verified by the next status probe), so forged blocks are pushed to it
  (`Broadcasting block ... to 1 peers`)
  instead of `0 peers`.
- Catch-up diagnostics: with ≤ 3 known peers every failed `getBlocks` is logged at WARN with the reason (timeout, empty
  reply, ...);
  on a tiny network a failing block source is parked for at most 5 s instead of 30 s -> 10 min.

## [0.13.0]

### Added SmartObject market (milestone `sobjV2`)

- **Sell order** (`action 4`, fee 1 STH): the owner sets `asset.price` (smartoshi) on any non-delegate SmartObject;
  `price 0` cancels.
  The order is stored in the SmartObject record (`price`, `forSale` in `/api/sobj*` and `/api/tokens*.SmartObject`).
- **Buy** (`action 5`, fee 1 STH): anyone pays the recorded price in the same transaction coins go to the current owner,
  the SmartObject (and, for a ticker, the token registry: owner, supply, manifest, mint / meta rights) goes to the
  buyer, the order closes. Atomic:
  no escrow, no trust. Checks: open order, buyer ≠ owner, buyer balance ≥ price + fee (`SmartObjectNotForSaleError`,
  `SmartObjectTransferToSelfError`, `SmartObjectInsufficientBalanceError`). Sell + buy + re-list inside one block
  resolve in order; rollback
  restores owner, order and balances (the previous owner comes from the undo snapshot, not from the transaction sender).
- Wire: `price` travels in the `ntfryData` slot as a decimal string SmartObject byte layout unchanged. `sth-core tx obj-sell --id X
  --price 2500`, `sth-core tx obj-buy --id X`. Tests `tests/sobj.rs::sobj_sell_and_buy`.
- `rules::check_sobj` takes a batch-aware `lookup(registrationId)` closure (record lives in the seller's wallet, not the
  buyer's).

## [0.12.0]

### Added

- **SmartObject transfer** (`typeGroup 2 / type 6`, `action 3`, milestone `sobjV2`): the owner hands an SmartObject and,
  for a type-5 ticker,
  the whole token registry (owner, supply, manifest, mint / meta rights) to `asset.recipientId`. Fee 5 STH. Token
  balances stay where
  they are. Refused for delegate SmartObjects and self-transfers; the old owner loses update / mint / meta rights
  immediately (also inside
  the same block: A->B->C in one block ends with C). On the wire the recipient travels in the `ntfryData` slot, so the
  byte layout of
  SmartObject transactions is unchanged. New index `eo:<id>` -> current owner (absent = registrant); rollback restores
  the previous owner
  and the `tk:` token index. `sth-core tx obj-transfer --id COFFEE --to D...`. Tests in `tests/sobj.rs`.
- Metrics page: **tokens tab** (node | tokens) token count / manifests / issue fee / activation status cards, token
  explorer table (logo from the chain, name + website, supply / cap bar, decimals, flags, owner, issue height) and the
  latest issues.
  Feed `GET /api/ntfry/tokens`; the dedicated metrics port also serves `/api/tokens/{id}/logo`.
- wallet guide for the "token showcase" screen (SVG/PNG upload rules, preview via
  `logoUrl`, error
  texts) and the ownership-transfer screen.
- `constants.sobjV2`, `constants.tokenMetaMaxLogoBytes`, `tokenFees.meta` in `/api/node/configuration`;
  `/api/transactions/types`  lists group 3 once tokens are active.

## [0.11.0]

### Added TokenMeta: the token manifest lives in the chain

- New typeGroup-3 type 4 **TokenMeta**:
  `asset.token.meta { name ≤ 64, description ≤ 512, website ≤ 128, logoType svg|png, logo base64 }`,
  logo file **≤ 8192 bytes** (PNG signature checked; SVG must be UTF-8 `<svg`/`<?xml`, no `<script`, `javascript:`,
  `<foreignObject>`).
  Owner only, token must be initialized, fee `tokenFees.meta` (default 1 STH), last one wins, rollback restores the
  previous manifest.
  Wire: `nameLen u8 | name | descLen u16 | desc | siteLen u8 | site | logoType u8 | logoLen u16 | logo`. Stored in
  `TokenState.meta`
  (owner wallet registry -> existing undo mechanism). Dormant on mainnet like the rest of typeGroup 3.
- API: `meta { name, description, website, logoType, logoSize, logoUrl }` in `/api/tokens*` and
  `attributes.tokensIssued`;
  `GET /api/tokens/{id}/logo` serves the image (`image/svg+xml` / `image/png`, cache + CSP headers). Logo bytes never
  appear in JSON.
- `sth-core tx ...` CLI signer for private networks / tests: `transfer`, `obj-register`, `obj-update`, `token-init`,
  `token-transfer` (`--to ADDR:AMOUNT` ×N), `token-mint`, `token-burn`, `token-meta --logo file.svg|png`. Human amounts
  with
  decimals, nonce / fees / ids resolved through the local API, `--dry-run` prints the signed JSON, passphrase via
  `STH_PASSPHRASE`.

### Changed SmartObjects

- JSON field `ipfsData` -> **`ntfryData`** everywhere (models, API, docs); the old key is still accepted on input. Wire
  bytes unchanged (the name never went on the wire; legacy peers receive serialized blocks). Mainnet has no SmartObject
  transactions yet, so no migration.
- Milestone **`sobjV2`** (default false; `init newnet` sets true): `ntfryData` = any UTF-8 text 1–255 bytes without
  control
  characters (legacy: base58 ≤ 128), and a type-5 (ticker) registration must match `^[A-Z0-9]{3,10}$`
  (`SmartObjectTickerInvalidError`) —
  the network enforces it instead of relying on wallet-side filters. Legacy rules stay in force on mainnet until the
  flag is set.

### Fixed

- Mempool rejected every TokenInit with `Transfer without recipient` (type 0 check ignored `typeGroup`).
- `/api/tokens/{key}`, `/holders`, `/api/wallets/{address}/tokens` returned 404: routes used `{key}` syntax instead of
  axum 0.7 `:key`.

## [0.10.1]

### Added `sth-core init newnet`: private / test networks in one command

- Generator (`src/newnet.rs`): `network.json` (title, ticker, `pubKeyHash`, nethash = genesis payload hash, burn
  address),
  `milestones.json` (mainnet rules collapsed into one milestone at height 1, `activeDelegates = N`, `tokens: false`,
  optional
  `--tokens-at H`), empty `exceptions.json`, a signed `genesisBlock.json` (treasury premine, stake + registration + vote
  for each
  delegate), `delegates.json` (passphrases, tests only) and a ready `node.yaml` (all N delegates forging,
  `network_dir: .`,
  separate ports 4002 / 4004 / 4889, `rest_nodes: []`, `legacy_peers: []`, iroh off). `--seed` makes the network
  deterministic.
- The node applies the genesis block from `network_dir` into an empty database before forging starts (no snapshot / REST
  needed).
- `delegate.quorum_share: 0.0` now explicitly means "private network": the forger takes its slot even when no peer
  answers
  `getStatus` (mainnet default 0.5 / `min_quorum_peers 3` unchanged an isolated mainnet node still skips its slot).
- Legacy P2P seed peers are only added for the network named `mainnet`; a private network never contacts mainnet nodes.
- `--config` is a global CLI flag: `info`, `wallet`, `rollback`, `snapshot import`, `sync` and `import-block` read
  `network` /
  `network_dir` / `db_path` from `node.yaml` instead of always assuming mainnet. A relative `network_dir` is resolved
  against the
  config file's directory, so `sth-core info --config /srv/testnet/node.yaml` works from any cwd.
- `Network::from_dir` fails on a missing directory instead of silently falling back to the embedded mainnet files.
- Tests `tests/newnet.rs`;

## [0.10.0]

### Added Native tokens (typeGroup 3), dormant until milestone `tokens: true`

- Transactions TokenInit / TokenTransfer (1–64 recipients, memo ≤ 64 B) / TokenMint / TokenBurn: models (`TokenAsset`),
  wire format (serializer + deserializer, ids and signatures round-trip), stateless rules (`rules::check_token_format`:
  exact static fees, ranges,
  flags) and wallet-aware rules (`rules::check_token` over a `TokenView`: ticker SmartObject type 5 owned by the sender,
  ticker
  `^[A-Z0-9]{3,10}$`, balances, owner-only mint within `supplyCap`, burnable flag).
- State: `WalletState.tokens` (balances) and `WalletState.tokens_issued` (registry `TokenState` in the owner wallet)
  covered by the
  existing undo/rollback; immutable indexes `tk:<id>` -> owner, `tks:<SYMBOL>` -> id (dropped when a TokenInit block is
  rolled back).
  50 % of the TokenInit fee is credited to the network burn address, the rest to the forger.
- Batch (`sync::check_stateful_rules`) and mempool views track pending token state; one TokenInit per id in the pool.
- Milestone fields `tokens`, `tokenFees {init, transfer, transferPerRecipient, mint, burn}`,
  `tokenTransferMaxRecipients` (default 64);
  defaults 100 / 0.1 / 0.01 / 1 / 0.1 STH.
- API: `GET /api/tokens`, `/api/tokens/{id|SYMBOL}`, `/api/tokens/{id}/holders`, `/api/wallets/{address}/tokens`;
  `attributes.tokens` / `attributes.tokensIssued` in wallets; `constants.tokens|tokenFees|tokenTransferMaxRecipients` in
  configuration.
- Tests `tests/tokens.rs`;
  `configuration.constants` also exposes `tokenTickerSobjType` (5) and `tokenTickerRule` (`^[A-Z0-9]{3,10}$`) for
  wallet-side validation.
  Mainnet behaviour unchanged: group 3 rejected before `H_TOKENS`.

## [0.9.9]

### Added

- `sth-core snapshot import --strict` **history audit**: every block of the dump is additionally passed through the live
  wallet-aware rules (`check_stateful_rules`: second signature, SHIP-13 SmartObjects) before being applied, so the whole
  chain can be
  replayed with exactly the checks the node applies to new blocks. Stops at the first block legacy accepted but we would
  reject.
- wallet-team guide for stage A (key derivation, commitment transaction, API, vectors, UX).

## [0.9.8]

### Added Quantum Shield stage A (no consensus change; legacy nodes see ordinary transfers)

- `crypto::pq`: **ML-DSA-44 (NIST FIPS 204)** via the RustCrypto `ml-dsa` crate deterministic key from the second
  passphrase (`seed = sha256("sth-pq-v1" || passphrase)`), pk 1 312 B, deterministic signatures 2420B with context
  `sth-pq-v1`, verify.
- **Commitment indexing**: a self-transfer (`recipientId == sender`, `amount ≥ 1`) whose `vendorField` is
  `sthpq1:<alg 2 hex>:<sha256(pq public key) 64 hex>` sets `wallet.pqCommitment {algorithm, commitment, height}`
  (last one wins; covered by undo/rollback like every wallet field). Foreign recipients and malformed strings are
  ignored.
- API: `/api/wallets/*` -> `quantumShield { committed, algorithm, commitment, height, active:false }`;
  `/api/node/configuration` -> `quantumShield { stage:"A", algorithms:[1], commitmentPrefix, activation:null }`;
  `/api/ntfry/metrics` -> `node.pqCommitments`; metrics page shows the number of committed wallets.
- Wallet-team vectors `tests/vectors/pq_v3.json` (passphrase -> seed -> pk -> commitment -> signature digest), tests
  `tests/pq.rs`.
- Stage B (v3 transactions with ML-DSA second signature, activation height) stays disabled until all active delegates
  run Rust.

## [0.9.7]

### Added

- Delegates dashboard: **legacy inference**. Live block intake now remembers, for the last 10 rounds, where each block
  first
  arrived from and who forged it. A delegate without a Rust announce whose observed blocks (≥2) all came through legacy
  peers
  that are not known Rust gateways is shown as `legacy` (inferred, with the block count); otherwise `unknown`.
  `/api/ntfry/delegates` adds `legacy`, `unknown` totals and per-delegate `inferred`, `observed`. `intake::origins()` /
  `intake::legacy_evidence()`; unit-tested.

## [0.9.6]

### Added

- **Delegates-on-Rust dashboard** (roadmap stage 1 progress). Rust nodes that forge publish a signed `Delegates` gossip
  announce every
  2 min: for each delegate key, an ECDSA signature over `sha256("sth-delegate-announce" || node id || timestamp)` replay
  to another
  node or a stale (>10 min) announce is rejected, nobody can claim a delegate they do not hold. Every Rust node keeps a
  registry (`p2p_iroh::delegates::RustDelegates`, entries stale after 15 min).
- `GET /api/ntfry/delegates` active delegate set (top-21 by votes) with `implementation: rust|unknown`, core version,
  node id,
  seconds since last announce; also in `/api/ntfry/metrics` -> `data.delegates {active, rust, list}`. Metrics page:
  progress bar
  "N of 21 active delegates on rust" + table.
- `delegate.announce` (default true) in node.yaml to opt out.
- Tests: `delegate_announce_is_bound_to_node_and_time`, `rust_delegates_propagate_over_gossip` (ignored, ~2 min, two
  nodes).

## [0.9.5]

### Fixed

- **Node stuck on a private fork after SHIP-13 height** (live report: local tip 11 802 295 `7940d64f` by delegate
  vs network
  `90d180d2`, 0 of 10 peers agree, catch-up looping on "peer blocks rejected"). Two root causes:
    1. `catch_up` had no fork handling a `previousBlock` mismatch at tip+1 was treated as a bad peer and the same range
       was
       re-requested forever. It now rolls back (depth 1 -> 3 -> 9 ... ≤ UNDO_DEPTH) and resumes from the new tip; undo
       records are kept
       during catch-up when ≤ 2 000 blocks behind. Test `catch_up_rolls_back_private_fork`.
    2. The forger accepted a quorum of 1–2 peers (e.g. only our own iroh nodes echoing our tip while legacy peers were
       unreachable)
       and forged 5 blocks alone over an hour. New `delegate.min_quorum_peers` (default 3): the slot is skipped unless
       that many
       peers confirm our exact tip; the skip reason now says how many were needed / "refusing to forge in isolation".

### Added

- `sth-core rollback --to <height>` manual rollback (stop the node first) for cases where automatic recovery is
  impossible.

## [0.9.4]

### Added

- **NETFORY n1 relays** built in: `https://relay-fsn7.sth.cx` and `https://relay-ru1.sth.cx` (iroh-relay) are always
  part of the
  relay map when `p2p.iroh.relay: true`. New options `relay_n0` (default true keep the public n0 relays too) and
  `relays: [...]`
  (extra iroh-relay URLs). Startup log lists the relay map; `/api/ntfry/*` meta shows `relays` + `homeRelays`, the
  metrics page
  shows the connected home relay. `RelaySetup` in `p2p_iroh`; live test `n1_relays_reachable` (ignored).

## [0.9.3]

### Fixed

- **OOM on long-running nodes** (n0-computer/iroh#4509, still present in iroh 1.2.0): `pending_open_paths` in iroh's
  `RemoteStateActor` grew geometrically with several QUIC connections to the same remote (gossip + RPC) until the
  allocator
  failed the node was killed by the kernel after hours on small VPS. iroh 1.1.0 is now vendored in `vendor/iroh`
  (`[patch.crates-io]`) with the retry queue deduplicated and bounded to 64 entries.

### Added

- `memory: rss N MB` INFO log every 10 min (Linux) and `node.rssBytes` in `/api/ntfry/metrics`; the metrics page shows
  RSS next to uptime.
- `db_cache_mb` in node.yaml (default 64): sled page cache size set 16–32 on a 1 GB VPS.
- Payout calculator: Robinhood Chain (L2, ETH gas) column; article `ARTICLE-BNB-SOLANA-STH_RU.md` now compares four
  networks.

## [0.9.2]

### Added

- Metrics page: **payout calculator** cost of N payouts as SmartHoldem multipayments vs single transfers vs BNB Chain vs
  Solana (editable STH price in USDT, BNB/Solana per-transfer fee estimates); fees and `multiPaymentLimit` are read live
  from the node.
- `/api/ntfry/metrics` -> `data.chain.multiPaymentLimit` and `data.chain.fees {transfer, multiPayment}` (smartoshi,
  current milestone).
- Docs: `MULTIPAY-1024.md` (blocktime x tx/block matrix for 1 024-recipient multipayments), `CONSENSUS.md`(finality explained, BFT-gadget protocol).

## [0.9.1]

### Added

- Every transaction entering the mempool (REST `POST /api/transactions`, legacy `postTransactions`, iroh gossip) is
  logged as one
  line: `Received transaction Transfer S... -> S... amount=1.50000000 STH fee=0.10000000 STH nonce=7 id=... memo="..."`
  (INFO);
  rejected ones as `Rejected transaction ... : ERR_* reason` (WARN); full JSON at DEBUG
  (`RUST_LOG=sth_core::mempool=debug`).
  Type-specific summaries for Vote, DelegateRegistration, MultiPayment (total), SmartObject actions, HTLC.

### Fixed

- **Gateway not visible on other Rust nodes / `neighbors: 0`.** (1) The gossip neighbour flag was a single boolean
  shared by the
  two topics, so a `NeighborDown` on the transactions topic hid a live blocks neighbour now tracked per topic. (2) A
  topic whose
  neighbours all dropped never re-bootstrapped: a watchdog re-joins the configured bootstrap peers and known RPC peers
  every 60 s
  while a topic has no neighbour. (3) The `Peers { gateway }` announcement is now broadcast on both topics (and handled
  on both),
  first 15 s after start instead of after 120 s. (4) `/api/ntfry/peers` and `/api/ntfry/metrics` report the node's
  **own**
  `gateway` address (`meta.gateway` / `data.ntfry.gateway`, `null` when not a gateway); the metrics page says
  "this node is a gateway (ip:port)". (5) `sth-core init` documents the gateway setup (`legacy_listen` +
  `legacy_public_addr`).
  Neighbour up/down events are logged at info level per topic.

## [0.9.0]

### Added SHIP-13 SmartObject transactions (consensus, activates at mainnet height 11 800 000)

- `network/mainnet/milestones.json`: `{ "height": 11800000, "sobj": true }` (SHIP-13 flag) same value the legacy
  `@smartholdem/crypto-networks`
  release must carry. Before that height SmartObject transactions are rejected
  (`SmartObject transaction before sobj activation`),
  legacy business/bridgechain types (group 2, types 0–5) are never accepted (none exist in the chain; legacy disables
  them at the same milestone).
- Wire format bit-compatible with legacy `@smartholdem/core` crypto (`typeGroup 2`, `type 6`):
  `type u8 | subType u8 | action u8 | regIdLen u8 | registrationId | nameLen u8 | name | ipfsLen u8 | ipfsData`
  (`crypto/tx_serializer.rs`, `tx_deserializer.rs`; JSON asset
  `{ type, subType, action, registrationId?, data: { name?, ipfsData? } }`).
- Rules exactly as the legacy SmartObject handler (`rules.rs::check_sobj_format / check_sobj`): amount 0, exact static
  fee (register 50 STH, update/resign 5 STH verified identical in the SmartHoldem fork), name
  `^[a-zA-Z0-9_!@$&.-]{1,40}$`,
  ipfsData base58 ≤ 128, network-wide unique `(name, type)` case-insensitive, Delegate SmartObject requires the sender's
  username,
  update/resign only by the owner with matching type/subType, no update/resign after resign. Legacy error names are
  kept.
- State: `WalletState.SmartObjects[<registrationId>] = { type, subType, data, resigned }` (`attributes.sobjects` in
  `/api/wallets`),
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
  `InvalidSecondSignatureError`), a wallet without one must not (`UnexpectedSecondSignatureError`), a second
  registration
  is rejected (`SecondSignatureAlreadyRegisteredError`), and the registration key must be a 33-byte compressed secp256k1
  key.
  Applied wallet-aware and in order inside `sync::apply_blocks` (registrations earlier in the same batch count) and in
  the
  mempool (a registration still pending in the pool already binds the sender). Until now the node only stored
  `secondPublicKey` and never checked it. Mainnet history has a single registration (height 8 948 990, no spend after
  it),
  so re-verification of the chain is unaffected. Tests: `tests/second_signature.rs`.

### Performance

- **Signature verification 8× faster** (`tests/bench_block.rs`, 4 vCPU): block of 500 transfers verify 411 -> 52 ms,
  10 000 transfers 8.2 s -> 1.0 s (verify + apply 9.1 s -> 1.95 s a 10k-tx block now fits an 8 s slot).
    - `crypto/schnorr.rs`: the quadratic-residue test used `num-bigint` modpow on every signature (≈ 0.5 ms); now k256's
      native `FieldElement::sqrt()`. `s·G − e·A` is one Shamir/Straus `lincomb_ext` instead of two scalar
      multiplications.
      Single-thread cost 0.82 -> 0.24 ms per signature. `num-bigint` dependency removed (`k256` feature `expose-field`).
    - `verify_block`: per-transaction id + signature checks run on all cores (`rayon`); `apply_blocks` verifies a whole
      catch-up batch (≤ 400 blocks) in parallel before the sequential link check and the sled write.
    - Catch-up with `sync.verify_blocks: true` is bound by peers, not CPU, at this point.

### Added

- **Operator metrics page** (`api.page_metrics: true`, default off): terminal-style dashboard height / network
  height / last-block age / uptime, legacy + ntfry peer counts, mempool, forging with next-slot countdown, last blocks,
  peer tables. Served at `http://<api host>:<port>/` or on its own address with `api.metrics_listen: "0.0.0.0:4888"`
  (that listener exposes only the page and `/api/ntfry/*`, nothing else from the API). `/` keeps `Hello World!` when
  off.
- `GET /api/ntfry/peers` Web 4.0 (iroh) peers by EndpointId only (never IPs): height, latency, neighbor/known,
  gateway flag; `meta.nodeId` is our own EndpointId. `GET /api/ntfry/metrics` one JSON snapshot for dashboards.
- **Block intake stats** (`intake.rs`, shown on the page as *block source* / *slot delay*): which channel delivered the
  last live block `pull/legacy` (follow loop), `push/legacy` (inbound `postBlock`), `gossip/iroh`, `forged` from
  which peer, and the delay between the block's slot start and its arrival (last, average of the last 100, per source).
  Exposed as `data.intake` in `/api/ntfry/metrics`.
- `tests/bench_block.rs` (ignored): block throughput probe sign / forge / serialize / verify / apply N transfers
  (`BLOCK_TXS=10000 cargo test --test bench_block -- --ignored --nocapture`). Dev profile now compiles k256 / sled with
  `opt-level = 3` so tests and probes measure real crypto speed.

### Changed

- `rewards.reward_passphrase` removed (it only derived an address that nothing uses; no reason to keep a second secret
  in the file old configs with the key still load). `rewards.reward_address` stays as a reserved field for future
  relay/gateway rewards; comments now say so explicitly and that block rewards always go to the forging delegate's
  wallet. The "no reward address configured" startup line is gone.

### Fixed

- **Catch-up stalled for minutes** (bursts of 400 blocks, then `0 blocks/sec`): block sources were ranked by
  `getStatus` latency (tens of ms) while `getBlocks` for 400 blocks takes 20 s+ on many legacy Node.js peers and hit the
  socket timeout; every failed attempt cost a worker 20 s, and the 60 s health refresh (`record_success` after a fast
  `getStatus`) reset the failure counter / ban, so the same slow peers were picked again.
  `PeerStats` now keeps a separate `getBlocks` metric measured speed (EMA per full range), consecutive failures and
  a parking timer (30 s -> 60 s -> ... -> 10 min) that status probes do not lift. `best_for_blocks()` ranks by that
  metric,
  skips parked peers, and the workers rather wait 150 ms for a fast idle peer than burn a timeout on a slow one.
- First `getBlocks` to a peer of unknown speed asks for 100 blocks (probe); subsequent requests are sized so the reply
  fits a 12 s budget (`blocks_limit()`, 50...400). Ranges are scheduled as `(from, to)`, so short replies re-queue only
  their tail no gaps, no overlaps.
- `GET /api/peers` legacy entries show `blocksLatency`, `blocksLimit`, `blocksFailures`, `blocksParkedFor`.
- **Legacy peers reset the next connection from the same IP for ~1 s after a `getBlocks` reply** (`Connection reset
  without closing handshake`): the per-peer spacing is now measured from the previous reply (1.5 s) instead of the
  request start. Measured on mainnet from genesis (debug build): 0 resets, ~1 150 blocks/s vs ~150 blocks/s and
  50 resets/100 s before. The live follow loop treats such a reset right after connecting as expected (no WARN, no
  penalty).

### Fixed

- **Forging order was wrong** (`delegate/round.rs::shuffle`): the legacy `shuffleDelegates` loop is
  `for (i...; i++) { for (x < 4 && i < n; i++, x++) ... }`, i.e. the outer `i++` skips indices 4, 9, 14, 19 after every
  batch of four swaps. Our port swapped every index, so the node predicted the wrong slot and never forged (the network
  showed an empty slot for `nicholasflamel`). Verified against mainnet round 557584: 21/21 generators
  now match; regression test `shuffle_matches_mainnet_round_557584`.
- `delegate.enabled: true` without a passphrase no longer aborts the node with
  `config error: ... no secrets configured`:
  the problem is reported as an ERROR with a ready-to-paste `delegate:` snippet and the node continues as a relay.
  Secrets are resolved before peer discovery, so the message appears immediately.
- `peers.json` download: the full error cause chain is logged (DNS / timeout / TLS instead of just
  `error sending request`), one retry after 2 s, and the fallback line states how many built-in seeds are used.
- Live follow: the socket reset legacy nodes perform right after a `getBlocks` reply is no longer logged as
  `WARN getStatus failed, switching peer` and no longer penalises the peer; `following chain via legacy peer` is logged
  only when the peer actually changes.

### Changed

- Delegate identity is printed at start-up from the chain state, one line per key:
  `Delegate nicholasflamel: address S..., rank 12 of 21 (active), public key 02c9...`. When the database is empty or
  still
  syncing, the forger re-resolves username / rank every 30 s until the delegate is found.

## [0.8.0]

### Added

- `GET /status` self-contained HTML status page on the local API (height, sync state, legacy + iroh peers with
  gateways, delegate slots / forging history, last blocks; refreshes every 5 s, no external assets).
- Gateway autodiscovery: `p2p.legacy_public_addr` ("ip:4001") is announced in the iroh `Peers` gossip message (`gateway`
  field, every 2 min); receivers add the gateway to their legacy peer table and list it in
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
- Console numbers grouped like the legacy core (`Received new block at height 11,708,910 ...`,
  `Broadcasting block 11,708,910 ...`).
- `GET /api/node/forging` delegate module state: configured delegates (username, rank, active), next own slot,
  last forged block (height, id, tx count, peers that accepted it), last 20 skipped slots with reasons.

## [0.5.0]

### Added

- **Iroh catch-up**: the download scheduler pulls 400-block ranges from iroh peers (fastest peer that has the range
  first, no per-second limit) alongside legacy IPs; `IrohNode::refresh_peers()` probes heights on start and every
  `p2p.refresh_secs`; `IrohNode::add_peer_addr()` / `MemoryLookup` for pinned LAN peers. Integration test: node B syncs
  two blocks forged by node A over iroh only.
- **Fast vote index** (`dv:<publicKey>` -> vote weight): updated incrementally on every wallet write and on rollback;
  `delegate_ranking()` is now O (delegates) instead of scanning all wallets; existing databases are migrated once at
  start (`ensure_vote_index`). `rebuild_vote_index()` for manual repair.
- Legacy-style console output: `Loaded N active delegate(s): name (publicKey)` (with a warning when the key is not
  registered or outside the top-21), `Next forging delegate ... is active on this node.`,
  `Forged new block ... by delegate ...`,
  `Broadcasting block H to N peers`, `Received new block at height H with T transactions from IP`,
  `Downloaded N new blocks ...`, `Blockchain 100% in sync`, `Starting Round R`.
- `GET /` -> `{ "data": "Hello World!" }` (legacy root probe used by netfory-provider) and legacy JSON 404 envelope for
  unknown routes.

- Forger quorum check: before forging, the best 8 legacy peers are asked for their tip; the node waits (until 2 s before
  the slot ends) for the previous slot's block to arrive and forges only when the majority reports our exact tip
  (height + id). Every owned slot is logged with the outcome (`Slot N belongs to delegate ...`,
  `Skipping slot N ...: reason`).

### Changed

- `P2pOptions` carries the optional iroh handle; block sources are `Source::Legacy(ip)` / `Source::Iroh(id)`.
- Live follow polls the best peer every 1.2 s instead of once per blocktime so new blocks are seen within ~1 s.

## [0.4.0]

### Added

- **Legacy API compatibility**: `GET /api/transactions/types`, `/api/transactions/schemas`,
  `/api/node/configuration/crypto`
  (network.json / milestones.json / exceptions.json / genesisBlock.json), `/api/peers/:ip`, `/api/votes`,
  `/api/votes/:id`,
  `/api/locks`, `/api/locks/:id`, `POST /api/locks/unlocked`, `/api/wallets/top`, `/api/wallets/:id/votes`,
  `/api/wallets/:id/locks`, `/api/sobj`, `/api/sobj/:id`, `/api/rounds/:round/delegates`.
- Full legacy filter set: transactions (`senderPublicKey`, `vendorField`, `version`, `sequence`,
  `timestamp/amount/fee/nonce`
  ranges), blocks (`generatorPublicKey`, `timestamp` ranges), wallets (`address`, `publicKey`, `balance`/`nonce` ranges,
  `orderBy`), delegates (`username`, `address`, `publicKey`, `isResigned`, numeric ranges, `orderBy`).
- `GET /api/node/peers` sth-core extension: live health table of legacy and iroh peers (latency history, height lag,
  score, failures, ban state, source).
- **Iroh layer** (`p2p_iroh/`): endpoint + `Router`, gossip topics `sth/<nethash>/blocks` and
  `sth/<nethash>/transactions`,
  JSON RPC on ALPN `sth/rpc/1` (`GetStatus`, `GetBlocks`), gap-fill from the announcing peer, mempool bridge (gossip ->
  mempool -> legacy `postTransactions` and back). `node.yaml` section `p2p.iroh` (`enabled`, `secret_key_file`,
  `bootstrap`, `serve_blocks`, `relay`), CLI `sth-core iroh-id`.
- **Delegate module** (`delegate/`): round/slot arithmetic and the legacy deterministic delegate shuffle, block assembly
  (fee-ordered, nonce-consistent, milestone limits), ECDSA signing, local apply, `postBlock` broadcast to legacy peers
  (+ iroh gossip). `node.yaml` section `delegate` (`enabled`, `secrets`, `secrets_file` = legacy `delegates.json`,
  `broadcast_fanout`). Round snapshots (`rd:<round>`) recorded at every round boundary.
- **Rollback**: undo records per block (`undo:<height>`, depth 1000) while following the chain; `rollback_last_block` /
  `rollback_to`; fork detection in the follow loop rolls back with growing depth and resyncs from the best peer.
- **Network files** in `crypto-networks` layout embedded (`network/mainnet/*.json`), `network_dir` in `node.yaml` and
  `sth-core init --network-files` to export/edit them (milestone-driven parameters, e.g. re-enabling rewards from block
  N).
- State for `DelegateResignation` (`resigned`), `HtlcLock` (`attributes.htlc.locks` / `lockedBalance`, `lk:` index),
  `MultiSignature` (combined-key address, `attributes.multiSignature`); Ipfs intentionally not tracked.
- `legacy_peer.post_block`, `serialize_block_with_transactions`, `PostBlockRequest/Response` protobuf messages.

### Changed

- `api.rs` split into `api/{mod,render,node,blocks,transactions,wallets,delegates,locks}.rs`.
- Catch-up is now pipelined: every worker fetches its next 400-block range as soon as it is free (shared scheduler with
  retry queue and per-peer 1.1 s spacing) instead of lock-step rounds ~400 blocks/s with 4 peers in the sandbox.
- `Network` is data-driven (milestones deep-merged like the legacy `configManager`); `Network::mainnet()` is a cheap
  clone of a cached instance (`mainnet_ref()`).
- `/api/node/fees` computes real avg/min/max/sum per type over the last `days`.
- `/api/node/status` and `/api/node/syncing` use the peer table's best height.

### Fixed

- Verifying a block re-parsed the embedded genesis for every transaction (`Network::mainnet()` in the serializer) —
  genesis verification took > 60 s; now ~4 s in debug.

## [0.3.0]

### Added

- `node.yaml` configuration (`sth-core init`, `run --config`; CLI flags override the file), sections
  `api / sync / p2p / rewards / mempool`; `rewards.reward_address` or `reward_passphrase` (address derived, logged at
  start).
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
