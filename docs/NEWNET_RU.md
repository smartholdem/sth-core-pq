# Своя сеть за одну команду: `sth-core init newnet`

Иногда нужно проверить идею, не трогая mainnet: обкатать нативные токены, прогнать Quantum Shield,
показать команде кошелька живую цепь или просто поиграть с делегатами на ноутбуке. Для этого в `sth-core`
есть генератор изолированных сетей. Одна команда - и у вас собственный блокчейн с генезисом, делегатами,
казной и готовым `node.yaml`. Mainnet при этом не затрагивается вообще: другой nethash, другой байт адреса,
другие порты, пустой список пиров.

## Быстрый старт

```bash
sth-core init newnet                 # диалог в терминале, ENTER = значение по умолчанию
cd smartholdem-testnet && sth-core run --config node.yaml
```

Диалог задаёт 13 вопросов (на английском), у каждого показан default:

| # | Вопрос | Default | Правило |
|---|---|---|---|
| 1 | `name` | `smartholdem-testnet` | только `a-z` и `-`; создаётся папка `./<name>` |
| 2 | `symbol` | `TSH` | символ в кошельках (`client.symbol`) |
| 3 | `token` | как symbol | тикер (`client.token`); **первая буква = первая буква всех адресов** |
| 4 | `messagePrefix` | `SmartHoldemTestNet message:\n` | `\n` = перевод строки |
| 5 | `pubKeyHash` | по первой букве token (`S` -> 63, `T` -> 65) | 0–255; буква не из base58 (`0 O I l`) -> 63 |
| 6 | `slip44` | `255` | 0–2147483647 (BIP-44 coin type) |
| 7 | `wif` | `255` | 0–255 |
| 8 | `Delegates` | `21` | 1–1000, все форжат на этой ноде |
| 9 | `P2P port` | `4002` | 1024–65535 |
| 10 | `API port` | `4004` | 1024–65535, не совпадает с P2P |
| 11 | `Metrics port` | `4889` | 1024–65535, не совпадает с предыдущими |
| 12 | `burnAddress` | генерируется сам: `TSHburnAddressXXX...` | без приватного ключа, начинается с буквы токена |
| 13 | `Activate all features? [Y/n]` | `Y` | Y - токены + Token Market, SmartObjects V2, strictBalance, Quantum Shield v3-транзакции, жёсткая финальность, Service Nodes с первых блоков; n - базовые правила mainnet |

В конце - сводка и `Create the network? [Y/n]`. Гибридные PQ-блоки (`pq.blocks`) не включаются автоматически: сначала
делегаты регистрируют PQ-ключи (`sth-cli pq-register`), затем `--pq-blocks-at H`.

Для скриптов и CI - без вопросов, все default или флаги:

```bash
sth-core init newnet --yes --delegates 3 --seed demo
sth-core init newnet -y --name my-net --token ABC --slip44 1 --all-features false --out ./net
```

Через 8 секунд первый блок сфоржен. Проверяем:

```bash
curl -s localhost:4004/api/blocks?limit=3
curl -s localhost:4004/api/delegates
open http://localhost:4889/          # страница метрик
```

Что напечатает генератор:

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

## Что лежит в папке сети

| Файл | Зачем |
|---|---|
| `network.json` | имя сети, тикер, `pubKeyHash`, `nethash` (= payload hash генезиса), burn-адрес |
| `milestones.json` | правила mainnet, свёрнутые в один milestone с высоты 1 (SHIP-11/13, HTLC, 8-секундный слот); `activeDelegates` = N; `tokens: false`; `sobjV2: true` (свободный `ntfryData`, тикер-имена для типа 5) |
| `exceptions.json` | пустой |
| `genesisBlock.json` | подписанный генезис: перевод в казну, стейк каждому делегату, N регистраций делегатов, N голосов |
| `delegates.json` | парольные фразы делегатов и казны - **только для тестов** |
| `node.yaml` | конфиг узла: `network_dir: .`, форжинг всеми N делегатами, `rest_nodes: []`, `legacy_peers: []`, iroh выключен |

## Параметры

| Флаг | По умолчанию | Смысл |
|---|---|---|
| `--out` | `./testnet` | куда писать файлы |
| `--ticker` / `--title` | `TSH` / `SmartHoldemTestNet` | тикер монеты и имя сети |
| `--delegates` | 21 | число активных делегатов (для ноутбука хватит 3) |
| `--pubkey-hash` | 30 | байт адреса: 30 -> адреса на `D...`, 63 -> как в mainnet на `S...` |
| `--p2p-port` / `--api-port` / `--metrics-port` | 4002 / 4004 / 4889 | порты - специально не совпадают с mainnet (4001 / 4003) |
| `--seed` | случайный | детерминированный seed: тот же seed -> те же ключи, тот же nethash |
| `--tokens-at H` | выкл | включить нативные токены (typeGroup 3) с высоты H |
| `--treasury` | 100 000 000 | премайн казны в целых монетах |
| `--delegate-stake` | 1 000 | +стейк каждого делегата в целых монетах к премайну|

Награда за блок в сгенерированных milestones равна `0` (как в текущем mainnet). Хотите проверить
начисление - впишите `"reward": "200000000"` в `milestones.json` до первого запуска.

## Как это работает внутри

1. **Генезис применяется сам.** При пустой БД узел берёт `genesisBlock.json` из `network_dir`, сверяет id
   с `network.json`, прогоняет через обычную проверку блока и записывает как высоту 1. Никаких снапшотов и
   REST-нод - для приватной сети им просто негде взяться.
2. **Форжинг без кворума.** В mainnet узел откажется форжить, если не видит ≥ 3 согласных пиров - так он
   не построит приватный форк. В `node.yaml` тестовой сети стоит `quorum_share: 0.0`, и это явный
   сигнал "сеть частная": слот форжится даже когда ни один пир не отвечает.
3. **Никаких mainnet-пиров.** Список seed-пиров legacy P2P подставляется только для сети с именем `mainnet`.
   Тестовая нода не постучится к `node0...node5.smartholdem.io` ни по REST, ни по WebSocket.
4. **`--config` - глобальный флаг.** `network_dir` в `node.yaml` разрешается относительно самого файла,
   поэтому `sth-core info --config /srv/testnet/node.yaml` покажет именно тестовую сеть, а не mainnet.
   `db_path` и другие пути по-прежнему относительны текущей директории - проще всего делать `cd` в папку сети.

## Транзакции из консоли: `sth-cli` / `sth-core tx`

С любого компьютера - `sth-cli --api http://<ip>:4004 ...` (см. `CLI_RU.md`); на машине узла - `sth-core tx ...` с теми же
подкомандами.

Кошелёк ещё не умеет typeGroup 3, поэтому в ноду встроен подписант. Парольная фраза остаётся в процессе CLI - нода получает
уже подписанный JSON через `POST /api/transactions`, как от любого кошелька.

```bash
cd testnet
export STH_PASSPHRASE="$(jq -r .treasury.passphrase delegates.json)"   # казна - на комиссии

sth-core tx transfer        --to DAqy... --amount 1000                       # 1000 монет, memo через --memo
sth-core tx obj-register    --name COFFEE                                  # тикер (sObject типа 5), 50 монет
sth-core tx token-init      --ticker COFFEE --decimals 2 --supply 1000000 --cap 5000000 --mintable --burnable
sth-core tx token-meta      --ticker COFFEE --name "Coffee Points" --description "Баллы лояльности" \
                            --website https://coffee.example --logo logo.svg   # SVG/PNG ≤ 8 КБ - в цепь
sth-core tx token-transfer  --ticker COFFEE --to DAqy...:150.25 --to DPzs...:20 --memo "первая партия"
sth-core tx token-mint      --ticker COFFEE --to DAqy... --amount 500
sth-core tx token-burn      --ticker COFFEE --amount 10
sth-core tx obj-update   --id COFFEE --data "sth://coffee/v2"           # строка-указатель ntfryData
sth-core tx obj-transfer --id COFFEE --to DPzs...                         # передать тикер и токен другому адресу (sobjV2)
sth-core tx obj-sell     --id COFFEE --price 2500                       # ордер на продажу (0 - снять)
STH_PASSPHRASE="<покупатель>" sth-core tx obj-buy --id COFFEE           # купить: цена уходит владельцу, объект - покупателю
```

Суммы - в целых единицах с учётом `decimals`. Nonce и комиссии подставляются сами (из `/api/wallets`, milestones), `--nonce` и `--api`
переопределяют. `--dry-run` печатает подписанную транзакцию вместо отправки - готовый эталон для команды кошелька. Токены нужно
включить (`--tokens-at`), иначе нода ответит `Token transactions are not activated yet`.

Проверка: `curl localhost:4004/api/tokens/COFFEE`, `.../holders`, логотип - `curl localhost:4004/api/tokens/COFFEE/logo`.

## Второй узел

Скопируйте папку сети на другую машину (или в соседний каталог), удалите из `node.yaml` секцию
`delegate.secrets`, смените порты, если узлы на одной машине, и добавьте первый узел в `p2p.legacy_peers`:

```yaml
p2p:
  legacy_port: 4002
  legacy_peers:
    - 192.168.1.10
```

Второй узел скачает цепь по legacy P2P и будет следовать за ней как обычный relay. Порт для `legacy_peers` берётся из
`p2p.legacy_port` (4002 для newnet) - указывать `ip:порт` не нужно; в логе такие пиры печатаются как `legacy:<ip>`.

**Оба узла должны быть одной версии `sth-core` и с одинаковым `milestones.json`.** Иначе relay отвергает блоки форжера -
типичный признак: `peer blocks rejected ...  Решение -
обновить первый узел и пересоздать тестовую сеть (`sth-core init newnet`), либо, если старую цепь нужно сохранить, прописать
в `milestones.json` обоих узлов те же `tokenFees`, по которым она форжилась (`{ "height": 1, "tokenFees": { "init": 10000000000 } }`).

## Что это даёт

- **Токены до `H_TOKENS`.** `--tokens-at 10` - и через полторы минуты можно выпускать TokenInit, гонять
  TokenTransfer на 64 получателя и смотреть, как откатывается TokenBurn. В mainnet всё это спит до общего
  перехода за 12,500,000 блок.
- **Кошелёк без риска.** Команда кошелька получает живой API с теми же JSON-ответами, что и в mainnet,
  и адреса на `D...`, которые невозможно спутать с настоящими.
- **Воспроизводимость.** Один `--seed` -> одна и та же сеть на любой машине. Баг-репорт "сеть `demo`,
  высота 120" воспроизводится у любого разработчика.

## Тесты

`tests/newnet.rs`: детерминизм по seed, загрузка сети из `node.yaml` из чужой директории, применение
генезиса в пустую БД, форжинг блока сгенерированным делегатом и перевод из казны с новым байтом адреса.
