# Your own network in one command: `sth-core init newnet`

Sometimes you need to test an idea without touching mainnet: try out native tokens, run Quantum Shield,
show the wallet team a live chain, or simply experiment with delegates on a laptop. For this, `sth-core`
provides a generator for isolated networks. One command is all it takes to create your own blockchain with a
custom genesis, delegates, treasury, and a ready-to-use `node.yaml`. Mainnet is not affected at all: different
nethash, different address byte, different ports, and an empty peer list.

## Quick start

```bash
sth-core init newnet                 # interactive terminal prompt, ENTER = default value
cd smartholdem-testnet && sth-core run --config node.yaml
```

The prompt asks 13 questions (in English), and each shows a default value:

| # | Question | Default | Rule |
|---|---|---|---|
| 1 | `name` | `smartholdem-testnet` | only `a-z` and `-`; creates a folder `./<name>` |
| 2 | `symbol` | `TSH` | symbol in wallets (`client.symbol`) |
| 3 | `token` | same as symbol | ticker (`client.token`); the first letter is the first letter of all addresses |
| 4 | `messagePrefix` | `SmartHoldemTestNet message:\n` | `\n` = newline |
| 5 | `pubKeyHash` | derived from the first letter of token (`S` -> 63, `T` -> 65) | 0–255; letters not in base58 (`0 O I l`) -> 63 |
| 6 | `slip44` | `255` | 0–2147483647 (BIP-44 coin type) |
| 7 | `wif` | `255` | 0–255 |
| 8 | `Delegates` | `21` | 1–1000; all forge on this node |
| 9 | `P2P port` | `4002` | 1024–65535 |
| 10 | `API port` | `4004` | 1024–65535, must differ from P2P |
| 11 | `Metrics port` | `4889` | 1024–65535, must differ from the previous ones |
| 12 | `burnAddress` | generated automatically: `TSHburnAddressXXX...` | no private key; starts with the token letter |
| 13 | `Activate all features? [Y/n]` | `Y` | Y = tokens + Token Market, SmartObjects V2, strictBalance, Quantum Shield v3 transactions, hard finality, Service Nodes from block 1; n = base mainnet rules |

At the end, you get a summary and `Create the network? [Y/n]`. Hybrid PQ blocks (`pq.blocks`) are not enabled automatically: first the delegates register PQ keys (`sth-cli pq-register`), then `--pq-blocks-at H`.

For scripts and CI, without interactive prompts, use defaults or flags:

```bash
sth-core init newnet --yes --delegates 3 --seed demo
sth-core init newnet -y --name my-net --token ABC --slip44 1 --all-features false --out ./net
```

After 8 seconds, the first block is forged. Check it:

```bash
curl -s localhost:4004/api/blocks?limit=3
curl -s localhost:4004/api/delegates
open http://localhost:4889/          # metrics page
```

What the generator prints:

```
new network written to ./smartholdem-testnet
  nethash     a6b9870d...22d100ad
  burn        TSHburnAddressXXXXXXXXXXXXXXYjZiMN
  genesis id  538d9acb...c1866037
  treasury    TKa83SCZbdBEKTnhgH8wrGdvFJFDVYhNKP  (passphrase in delegates.json)
  delegates   3 (passphrases in delegates.json, all forging in node.yaml)
  seed        demo
run:  cd ./smartholdem-testnet && sth-core run --config node.yaml
```

## What is inside the network folder

| File | Purpose |
|---|---|
| `network.json` | network name, ticker, `pubKeyHash`, `nethash` (= genesis payload hash), burn address |
| `milestones.json` | mainnet rules folded into a single milestone starting at height 1 (SHIP-11/13, HTLC, 8-second slot); `activeDelegates` = N; `tokens: false`; `sobjV2: true` (free `ntfryData`, ticker names for type 5) |
| `exceptions.json` | empty |
| `genesisBlock.json` | signed genesis: treasury transfer, stake for each delegate, N delegate registrations, N votes |
| `delegates.json` | delegate and treasury passphrases - for testing only |
| `node.yaml` | node config: `network_dir: .`, forging with all N delegates, `rest_nodes: []`, `legacy_peers: []`, iroh disabled |

## Parameters

| Flag | Default | Meaning |
|---|---|---|
| `--out` | `./testnet` | where to write the files |
| `--ticker` / `--title` | `TSH` / `SmartHoldemTestNet` | coin ticker and network name |
| `--delegates` | 21 | number of active delegates (3 is enough for a laptop) |
| `--pubkey-hash` | 30 | address byte: 30 -> addresses starting with `D...`, 63 -> like mainnet with `S...` |
| `--p2p-port` / `--api-port` / `--metrics-port` | 4002 / 4004 / 4889 | ports purposely different from mainnet (4001 / 4003) |
| `--seed` | random | deterministic seed: same seed -> same keys, same nethash |
| `--tokens-at H` | off | enable native tokens (typeGroup 3) from height H |
| `--treasury` | 100 000 000 | treasury premine in whole coins |
| `--delegate-stake` | 1 000 | +stake for each delegate in whole coins added to the premine |

The block reward in generated milestones is `0` (same as current mainnet). If you want to test reward accrual,
add `"reward": "200000000"` to `milestones.json` before the first launch.

## How it works internally

1. **Genesis is applied automatically.** With an empty database, the node reads `genesisBlock.json` from `network_dir`, verifies its id against `network.json`, passes it through the normal block verification flow, and stores it as height 1. There are no snapshots or REST nodes for a private network because there is nowhere for them to come from.
2. **Forging without quorum.** On mainnet, the node refuses to forge if it does not see at least 3 agreeing peers, so it cannot build a private fork. In the test network `node.yaml`, `quorum_share: 0.0` is set, which is a clear signal that the network is private: a slot is forged even when no peers respond.
3. **No mainnet peers.** The list of legacy P2P seed peers is only used for a network named `mainnet`. A test node will never call `node0...node5.smartholdem.io`, either via REST or WebSocket.
4. **`--config` is a global flag.** `network_dir` in `node.yaml` is resolved relative to the file itself, so `sth-core info --config /srv/testnet/node.yaml` shows the test network rather than mainnet. `db_path` and other paths remain relative to the current working directory, so the simplest approach is to `cd` into the network folder.

## Transactions from the console: `sth-cli` / `sth-core tx`

From any machine: `sth-cli --api http://<ip>:4004 ...` (see `CLI_RU.md`); on the node machine: `sth-core tx ...` with the same subcommands.

The wallet does not yet support typeGroup 3, so a signer is built into the node. The passphrase remains in the CLI process; the node receives the already signed JSON through `POST /api/transactions`, exactly as it would from any wallet.

```bash
cd testnet
export STH_PASSPHRASE="$(jq -r .treasury.passphrase delegates.json)"   # treasury pays the fee

sth-core tx transfer        --to DAqy... --amount 1000                       # 1000 coins, memo via --memo
sth-core tx obj-register    --name COFFEE                                  # ticker (sObject type 5), 50 coins
sth-core tx token-init      --ticker COFFEE --decimals 2 --supply 1000000 --cap 5000000 --mintable --burnable
sth-core tx token-meta      --ticker COFFEE --name "Coffee Points" --description "Loyalty points" \
                            --website https://coffee.example --logo logo.svg   # SVG/PNG ≤ 8 KB - included in-chain
sth-core tx token-transfer  --ticker COFFEE --to DAqy...:150.25 --to DPzs...:20 --memo "first batch"
sth-core tx token-mint      --ticker COFFEE --to DAqy... --amount 500
sth-core tx token-burn      --ticker COFFEE --amount 10
sth-core tx obj-update   --id COFFEE --data "sth://coffee/v2"           # string pointer ntfryData
sth-core tx obj-transfer --id COFFEE --to DPzs...                         # transfer the ticker and token to another address (sobjV2)
sth-core tx obj-sell     --id COFFEE --price 2500                       # sell order (0 = cancel)
STH_PASSPHRASE="<buyer>" sth-core tx obj-buy --id COFFEE           # buy: price goes to the owner, object goes to the buyer
```

Amounts are in whole units, adjusted for `decimals`. Nonce and fees are filled in automatically (from `/api/wallets`, milestones), and `--nonce` and `--api` can override them. `--dry-run` prints the signed transaction instead of sending it - a ready-made example for the wallet team. Tokens must be activated (`--tokens-at`); otherwise the node replies `Token transactions are not activated yet`.

Validation: `curl localhost:4004/api/tokens/COFFEE`, `.../holders`, logo: `curl localhost:4004/api/tokens/COFFEE/logo`.

## Second node

Copy the network folder to another machine (or to a sibling directory), remove the `delegate.secrets` section from `node.yaml`, change ports if both nodes are on the same machine, and add the first node to `p2p.legacy_peers`:

```yaml
p2p:
  legacy_port: 4002
  legacy_peers:
    - 192.168.1.10
```

The second node downloads the chain over legacy P2P and follows it as a normal relay. The port for `legacy_peers` is taken from `p2p.legacy_port` (4002 for newnet); you do not specify `ip:port`. In the logs, such peers appear as `legacy:<ip>`.

**Both nodes must run the same version of `sth-core` and use the same `milestones.json`.** Otherwise the relay rejects the forger's blocks - a typical sign is: `peer blocks rejected ...` Solution: update the first node and recreate the test network (`sth-core init newnet`), or, if you must keep the old chain, set the same `tokenFees` in both nodes' `milestones.json`, the ones the chain was forged with (`{ "height": 1, "tokenFees": { "init": 10000000000 } }`).

## What this gives you

- **Tokens before `H_TOKENS`.** `--tokens-at 10` - and within a minute and a half, you can issue `TokenInit`, run `TokenTransfer` to 64 recipients, and watch `TokenBurn` roll back. On mainnet, all of this remains dormant until the shared migration at block 12,500,000.
- **Wallet without risk.** The wallet team gets a live API with the same JSON responses as mainnet, and addresses starting with `D...`, which are impossible to confuse with real ones.
- **Reproducibility.** One `--seed` -> the same network on any machine. A bug report like "network `demo`, height 120" is reproducible for every developer.

## Tests

`tests/newnet.rs`: deterministic behavior by seed, loading a network from `node.yaml` in another directory, applying genesis to an empty database, forging a block with a generated delegate, and transferring from the treasury using a new address byte.
