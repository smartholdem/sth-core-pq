# SPEC-PQ-V3 - формат постквантовых транзакций SmartHoldem (версия 3)

Статус: **draft 1** для команды кошелька. Ядро: `sth-core` (Rust). Все числа little-endian, если не указано иное;
`hex` - строчные символы без префикса. Байтовые размеры даны для ML-DSA-44.

Связанные документы: `PLAN-QUANTUM-SHIELD.md` (зачем и этапы), `DEV-DOC-TRANSACTIONS.md` (текущий формат v2).

---

## 1. Обзор

- Версия транзакции `3` = "PQ-capable". Заголовок, полезная нагрузка типов и **первая подпись (secp256k1 Schnorr) не меняются**.
- Меняется только **секция вторых подписей**: вместо одной 64-байтной подписи - список блоков `alg || len || sig`.
- Регистрация второй подписи (type 1) в v3 несёт постквантовый открытый ключ вместо 33-байтного secp256k1.
- Адрес, `senderPublicKey`, nonce, комиссии, `vendorField` - как в v2. Пользователь **не меняет кошелёк**.
- До активации (высота `H`, milestone `pq.activation`) сеть транзакции v3 отвергает. До `H` доступен только **commitment** (§7) -
  обычный перевод v2, валидный для всех нод.

## 2. Алгоритмы и идентификаторы

| `alg_id` | схема | стандарт | pk | sk (seed) | подпись | статус |
|---:|---|---|---:|---:|---:|---|
| `0x00` | secp256k1 Schnorr (legacy, bcrypto) | - | 33 | 32 | 64 | только для "доказательства старого ключа" при миграции (§5.2) |
| `0x01` | **ML-DSA-44** | NIST FIPS 204 | 1 312 | 32 (ξ) | 2 420 | **обязателен к поддержке** |
| `0x02` | FN-DSA-512 (Falcon) | FIPS 206 (draft) | 897 | 32 | ≤ 666 | зарезервирован, пока не активирован |
| `0x03` | SLH-DSA-SHA2-128s | FIPS 205 | 32 | 32 | 7 856 | зарезервирован |
| `0x04–0x7e` | - | - | | | | резерв |
| `0x7f`, `0xff` | - | - | | | | запрещены (`0xff` - маркер мультиподписи v2) |

Список разрешённых `alg_id` публикует нода: `GET /api/node/configuration` → `data.pq.algorithms` (например `[1]`).

## 3. Вывод ключей из passphrase

Вторая passphrase (любая строка UTF-8, обычно 12 слов BIP-39) остаётся единственным секретом.

```
xi  = SHA256( UTF8("sthpq1") || alg_id(1 байт) || UTF8(second_passphrase) )      // 32 байта
(pk, sk) = ML-DSA-44.KeyGen_internal(xi)                                           // FIPS 204, Algorithm 6
```

- Хранить достаточно passphrase (или `xi`); `sk` (2 560 Б) всегда восстанавливается детерминированно.
- JS: `@noble/post-quantum` → `ml_dsa44.keygen(xi)` → `{ publicKey: 1312 B, secretKey: 2560 B }` (keygen из seed = KeyGen_internal).
- Rust (ядро): крейт `ml-dsa` → `MlDsa44::key_gen_internal(&xi)`.
- **Требование**: JS-кошелёк и ядро обязаны выдавать бит-в-бит одинаковые `pk` из одного `xi` (см. тест-векторы §10).
- Первая passphrase → secp256k1, как сейчас: `priv = SHA256(passphrase)`.

Не путать с legacy второй подписью: в v2 `secondPublicKey = secp256k1(SHA256(passphrase))`. Один и тот же текст passphrase даёт
разные ключи для `alg_id 0x00` и `0x01` - это нормально; при миграции (§5.2) кошелёк вычисляет оба.

## 4. Wire-формат транзакции v3

### 4.1 Заголовок и тело (без изменений относительно v2)

| поле | размер | значение |
|---|---:|---|
| marker | 1 | `0xff` |
| version | 1 | **`0x03`** |
| network | 1 | `0x3f` (63, mainnet) |
| typeGroup | 4 | `1` |
| type | 2 | тип (0 transfer, 1 secondSignature, 2 delegateRegistration, 3 vote, 6 multiPayment, …) |
| nonce | 8 | nonce отправителя (последний + 1) |
| senderPublicKey | 33 | secp256k1 compressed |
| fee | 8 | smartoshi |
| vendorField | 1 + n | `len(u8) || utf8`, `0x00` если нет; только для type 0/6/8 |
| payload | var | как в v2, **кроме type 1** (см. 4.2) |

### 4.2 Полезная нагрузка type 1 (secondSignature) в v3

```
alg_id      u8
pk_len      u16 LE          // 1312 для ML-DSA-44
public_key  pk_len байт
```

JSON:
```json
"asset": { "signature": { "algorithm": 1, "publicKey": "<hex 1312 байт>" } }
```
В v2 `asset.signature.publicKey` - 33-байтный secp256k1 hex; в v3 поле `algorithm` обязательно, `publicKey` - сырой PQ-ключ (без `alg_id`).

### 4.3 Секция подписей v3

```
signature          64 байта      // secp256k1 Schnorr (legacy bcrypto), как в v2
second_blocks*     повтор        // 0..N блоков:  alg_id u8 || sig_len u16 LE || signature
[ 0xff  multisig ] как в v2      // опционально, только если кошелёк мультиподписной
```

Разбор: после первой подписи читать байт `b`:
- `b == 0xff` → мультиподписи v2 (без изменений);
- `0x00 ≤ b ≤ 0x7e` → блок второй подписи, читать `sig_len`, `signature`, повторить;
- конец буфера → вторых подписей нет.

JSON (заменяет строку `secondSignature` из v2):
```json
"secondSignatures": [
  { "algorithm": 0, "signature": "<hex 64 байта>" },
  { "algorithm": 1, "signature": "<hex 2420 байт>" }
]
```
Порядок блоков - **по возрастанию `algorithm`**, дубликаты запрещены. Поля `secondSignature` / `signSignature` в v3 отсутствуют.

## 5. Что подписывается

Обозначения: `BODY` = байты 4.1–4.2; `SIG1` = первая подпись.

| подпись | сообщение | алгоритм |
|---|---|---|
| первая (`signature`) | `SHA256(BODY)` | secp256k1 Schnorr legacy (как в v2) |
| каждый блок второй подписи | `M2 = SHA256(BODY || SIG1)` | по `alg_id`: `0x00` Schnorr(M2); `0x01` ML-DSA-44.Sign(sk, M2, ctx = "") |

- ML-DSA: контекстная строка **пустая**; сообщение - ровно 32 байта `M2`. Разрешён и hedged (по умолчанию в noble), и
  детерминированный режим - проверка одинакова.
- `id` транзакции = `SHA256(полные байты, включая все подписи)`, hex - как в v2.
- Мультиподписи (если есть) подписывают `SHA256(BODY)` как в v2 и в `M2` не входят.

### 5.1 Правило проверки для кошелька с PQ-ключом

Если у кошелька-отправителя зарегистрирован `pqPublicKey (alg A)`, **каждая** его транзакция (любого типа) обязана содержать ровно
один блок с `alg_id = A`, валидный по `pqPublicKey`. Иначе - `ERR_PQ_SECOND_SIGNATURE_REQUIRED` / `_INVALID`. Классическая вторая подпись
(`alg 0x00`) после активации PQ у этого кошелька больше не требуется и не принимается (кроме §5.2).

### 5.2 Регистрация и миграция (type 1, v3)

| состояние кошелька | требуемые блоки в регистрирующей tx |
|---|---|
| нет второй подписи | `[alg_new]` - подпись **новым** PQ-ключом (доказательство владения) |
| есть legacy вторая подпись (secp256k1) | `[0x00 старым secp256k1-ключом, alg_new новым PQ-ключом]` |
| есть PQ-ключ `A`, ротация на `B` (или тот же alg, новый ключ) | `[A старым ключом, B новым ключом]`; если `A == B`, оба блока с одним `alg_id` - единственное исключение из "без дубликатов": первый = старый ключ, второй = новый |

После включения в блок `wallet.secondPublicKey` (legacy) очищается, `wallet.pq = { algorithm, publicKey, since: height }`.
Комиссия: `staticFees.secondSignature` (5 STH) + per-byte (§8).

## 6. Валидация на ноде (порядок)

1. `version == 3` разрешён только при `height ≥ pq.activation`; `alg_id ∈ pq.algorithms`.
2. Размеры: `pk_len`, `sig_len` строго равны табличным для `alg_id` (Falcon - ≤ 666, паддинг по FIPS 206).
3. Первая подпись верна (как v2).
4. Блоки вторых подписей: порядок, дубликаты, правило §5.1/§5.2, каждый блок проверен по своему ключу.
5. type 1 v3: если у кошелька есть commitment (§7) и `height < pq.activation + pq.commitmentGrace`, то `SHA256(publicKey)` **обязан**
   совпадать с последним commitment, зафиксированным до `pq.commitmentFreeze`. Без commitment - регистрация разрешена.
6. Остальное - как в v2 (nonce, баланс, комиссия, лимиты типа).

Коды ошибок (`/api/transactions` → `errors[id][].type`): `ERR_PQ_NOT_ACTIVE`, `ERR_PQ_ALGORITHM`, `ERR_PQ_LENGTH`,
`ERR_PQ_SECOND_SIGNATURE_REQUIRED`, `ERR_PQ_SECOND_SIGNATURE_INVALID`, `ERR_PQ_COMMITMENT_MISMATCH`, `ERR_PQ_LEGACY_PROOF_REQUIRED`.

## 7. Commitment (этап A - работает уже сейчас, без изменений консенсуса)

Обычный перевод v2 **самому себе** с `vendorField`:

```
sthpq1:<alg_id hex, 2 символа>:<SHA256(publicKey) hex, 64 символа>      // 74 байта ASCII
пример: sthpq1:01:3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca046dea
```

- `amount` ≥ 1 smartoshi, `recipientId == адрес отправителя`, комиссия обычная (1 STH). Legacy-ноды видят обычный перевод.
- Ядро индексирует: `GET /api/wallets/{address}` → `"quantumShield": { "committed": true, "algorithm": 1, "commitment": "<hex64>", "height": N }`.
- Побеждает **последний** commitment с высотой `< pq.commitmentFreeze` (ориентир: `H − 86 400` блоков ≈ 8 суток). Более поздние
  игнорируются - окно, в котором атакующий не может подменить чужой commitment.
- Кошелёк UX: "Quantum Shield → зафиксировать ключ" = сгенерировать PQ-ключ из второй passphrase, отправить commitment, показать статус.
  Регистрация (type 1 v3) станет доступна после `H`.

## 8. Размеры и комиссии

| транзакция | размер v2 | размер v3 с ML-DSA-44 |
|---|---:|---:|
| transfer без второй подписи | ~160 Б | 160 Б (v3 не нужна) |
| transfer, PQ-locked кошелёк | - | ~2 580 Б (160 + 3 + 2 420) |
| регистрация PQ (нет legacy 2-й подписи) | - | ~3 860 Б (59 + 1 315 + 64 + 2 423) |
| регистрация PQ с legacy 2-й подписью | - | ~3 930 Б |
| vote, PQ-locked | ~200 Б | ~2 620 Б |

Комиссия v3 (параметры milestone, значения ориентировочные, финализируются перед `H`):
```
fee_min = staticFees[type] + pq.feePerByte × size_bytes        // pq.feePerByte ≈ 10 000 smartoshi/байт → +0,26 STH за transfer
```
Мемпул принимает v3-tx с `fee ≥ fee_min`; лимит мемпула - по байтам (`mempool.max_bytes`), а не только по количеству.

## 9. Доступность через API

- `GET /api/node/configuration` → `data.pq: { "activation": H, "algorithms": [1], "feePerByte": …, "commitmentFreeze": F }`
  (до активации `activation: null`, `algorithms: []`).
- `GET /api/wallets/{id}` → `quantumShield: { committed, algorithm, commitment, height, active, publicKey?, since }`.
- `POST /api/transactions` - как сегодня, тело v3 в JSON (§4). Максимальный размер запроса будет поднят до 1 МБ.
- `GET /api/transactions/{id}` возвращает `secondSignatures` для v3; `version: 3`.

## 10. Тест-векторы (публикуются вместе с этапом A в `tests/vectors/pq_v3.json`)

Обязательные кейсы, которые кошелёк должен воспроизвести бит-в-бит:
1. `xi` из passphrase `"this is a top secret passphrase"`, `alg 0x01` → `pk` (1312 Б, hex), `SHA256(pk)`.
2. Commitment-tx v2 (§7) для этого ключа: полные байты, `id`.
3. Регистрация v3 без legacy 2-й подписи: `BODY`, `SIG1`, `M2`, `pk`, детерминированная ML-DSA-подпись, полные байты, `id`.
4. Регистрация v3 с legacy 2-й подписью (`alg 0x00` + `0x01`).
5. Transfer v3 от PQ-locked кошелька (nonce 3, amount 1 STH, vendorField "pq").
6. Негативные: неверный порядок блоков, блок без регистрации, `alg` вне списка, неверная длина, v3 до активации.

Пока векторы не опубликованы, кошелёк проверяет совместимость по NIST KAT для ML-DSA-44 (ACVP) и по формуле `xi` из §3.

## 11. Совместимость и переход

- Кошелёк обязан продолжать формировать **v2** для всех обычных операций; v3 - только для PQ-регистрации и для кошельков с активным PQ.
- Определять состояние отправителя перед подписью: `GET /api/wallets/{addr}` → если `quantumShield.active` → строить v3 с блоком `alg`.
- Legacy-ноды (Node.js) v3 не понимают: активация `H` назначается, когда все активные делегаты на Rust-ядре.
- Форматы блоков и делегатских подписей этап B не меняет (см. этап C в плане).

## 12. Открытые вопросы к команде кошелька

1. Хранение `xi`/passphrase: держать ли кэш `sk` (2,5 КБ) в памяти сессии или пересчитывать (keygen ~0,1 мс) - рекомендуем пересчитывать.
2. Нужна ли поддержка аппаратных кошельков (Ledger не умеет ML-DSA) - тогда PQ только для software-кошельков на первом этапе.
3. UX предупреждение: потеря второй passphrase после активации PQ = потеря доступа (как и сегодня со второй подписью).
