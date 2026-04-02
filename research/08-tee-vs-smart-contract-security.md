# TEE vs Smart Contract: почему нас нельзя ограбить как Drift

**Дата:** 2026-04-02
**Контекст:** Drift Protocol (Solana perp DEX) потерял $200M+ из-за компрометации приватного ключа администратора

---

## Что произошло с Drift (детальный анализ)

Атака была значительно сложнее чем "кража ключа". Drift **имел multisig** (Security Council, 5 подписантов) — но это не спасло.

**Хронология:**

1. **23 марта:** Атакующий создал 4 кошелька с **durable nonces** (механизм Solana для отложенного исполнения транзакций). Два кошелька были привязаны к членам Security Council.
2. **30 марта:** Drift провёл ротацию Security Council. Атакующий адаптировался — создал новый кошелёк, соответствующий обновлённым параметрам multisig.
3. **1 апреля:** Drift провёл легитимный тест вывода из insurance fund. **В течение ~1 минуты** атакующий активировал две предварительно авторизованные транзакции, получив административные права.
4. **Вывод:** $155.6M JPL, $60.4M USDC, $11.3M CBBTC, $4.7M WETH, $4.5M DSOL, $4.4M WBTC, $4.1M FARTCOIN и другие. **Итого: ~$280M.**

**Как прошла атака через multisig:**

Атакующий использовал **social engineering** — убедил минимум 2 из 5 подписантов Security Council одобрить транзакции. Durable nonces позволили подготовить транзакции заранее и исполнить автоматически.

**Корневые причины:**
- Подписанты multisig — **люди**, подверженные social engineering
- Durable nonces позволяют **pre-sign** транзакции без немедленного исполнения
- Нет верификации что подписанная транзакция **разумна** (margin check, amount limit)
- Мониторинг не обнаружил подготовку за неделю до атаки

---

## Почему это невозможно в нашей архитектуре

### 0. Подписанты — hardware, не люди

```
Drift multisig:                  Наша архитектура:
┌──────────┐                     ┌──────────────────┐
│ Человек 1│ ← social           │ SGX Enclave A    │
│ Человек 2│    engineering     │ SGX Enclave B    │
│ Человек 3│    возможен        │ SGX Enclave C    │
│ Человек 4│                     │                  │
│ Человек 5│                     │ (hardware, не    │
└──────────┘                     │  поддаётся       │
     │                           │  уговорам)       │
  2 of 5 убеждены →             └──────────────────┘
  полный доступ                        │
                                 Enclave подпишет ТОЛЬКО
                                 если margin check пройден
                                 и tx валидна по коду
```

**Drift's multisig was defeated by social engineering.** Атакующий убедил 2 человек подписать. В нашей архитектуре подписанты — SGX enclaves. Нельзя "убедить" процессор подписать невалидную транзакцию.

### 1. Ключ не существует вне SGX

```
Drift:                           Наша архитектура:
┌──────────┐                     ┌──────────────────┐
│ Admin key│ ← хранится          │ SGX Enclave      │
│ в файле/ │    где-то           │ ┌──────────────┐ │
│ в HSM/   │    доступном        │ │ ECDSA Key A  │ │
│ в памяти │    оператору        │ │ (sealed,     │ │
└──────────┘                     │ │  never leaves│ │
     │                           │ │  enclave)    │ │
     │ украден →                 │ └──────────────┘ │
     │ полный доступ             └──────────────────┘
     ▼                                    │
  $200M вывод                     Оператор НЕ МОЖЕТ
                                  извлечь ключ
```

В SGX приватный ключ **генерируется внутри enclave** и **никогда не покидает** его. Оператор запускает enclave, но физически не может прочитать содержимое enclave memory — это гарантия на уровне процессора Intel.

### 2. Multisig 2-of-3 — нет единого ключа

```
Drift: 1 admin key → полный контроль

Наша архитектура:
  Operator A (Azure): ECDSA Key A — внутри SGX
  Operator B (Azure): ECDSA Key B — внутри SGX
  Operator C (Azure): ECDSA Key C — внутри SGX

  XRPL Escrow: SignerListSet [A, B, C], quorum=2
  Master key: DISABLED

  Для любого withdrawal нужно 2 из 3 подписей.
  Каждый ключ внутри своего SGX enclave.
  Операторы на разных серверах, разных провайдерах.
```

Даже если атакующий **полностью скомпрометирует** один сервер (root доступ, физический доступ) — он получит доступ только к одному enclave. Для вывода средств нужно скомпрометировать **два enclave на двух разных серверах**.

### 3. Enclave код определяет правила — оператор не может их обойти

```
Drift: admin key может делать что угодно
       (перевести все средства на свой адрес)

Наша архитектура:
  Enclave код (attested, open-source):
    - Withdrawal только после margin check
    - Signing только для конкретного user + amount
    - Rate limit на withdrawals
    - Spending guardrails (signature count limit)

  Оператор НЕ МОЖЕТ заставить enclave подписать
  произвольную транзакцию — код enclave это запрещает.
```

### 4. DCAP Attestation — код верифицирован

```
Drift: пользователи доверяют что smart contract делает
       то что написано (но admin key обходит всё)

Наша архитектура:
  1. Enclave публикует MRENCLAVE (хеш кода)
  2. Intel подписывает SGX Quote (DCAP)
  3. Любой может верифицировать:
     - Код enclave = опубликованный open-source код
     - Работает на настоящем Intel SGX
     - Оператор не модифицировал код

  Если оператор попытается запустить модифицированный
  enclave — MRENCLAVE изменится → attestation провалится
  → пользователи увидят подмену
```

### 5. XRPL Settlement — средства на L1, не в контракте

```
Drift: все средства внутри smart contract на Solana
       admin key = полный доступ к контракту = полный доступ к средствам

Наша архитектура:
  Средства: RLUSD на XRPL escrow account
  Контроль: SignerListSet 2-of-3 (не smart contract)

  XRPL — фиксированный протокол, нет upgradeable contracts.
  SignerListSet — нативная фича XRPL, не наш код.
  Нет admin key, нет upgrade function, нет proxy pattern.
```

---

## Сравнительная таблица атак

| Вектор атаки | Drift ($280M, реальная атака) | Наша архитектура (TEE + Multisig) |
|---|---|---|
| **Social engineering multisig** | ✅ 2 из 5 людей убеждены подписать | ❌ Подписанты = SGX hardware, не люди |
| **Durable nonces (pre-signed tx)** | ✅ Транзакции подготовлены за неделю | ❌ Enclave подписывает только в момент запроса, каждый раз с margin check |
| **Rehearsal (подготовка)** | ✅ Тестовые кошельки, адаптация к ротации | ❌ DCAP attestation — код неизменяем, нет "адаптации" |
| **Timing attack** | ✅ Атака во время легитимной операции | ❌ Enclave не различает "легитимное" и "атаку" — проверяет одинаково |
| **Admin key compromise** | ✅ Полный контроль через Council | ❌ Нет admin key. Master key disabled на XRPL |
| **Insider threat** | ✅ Члены Council = потенциальные инсайдеры | ❌ Операторы не имеют доступа к ключам (SGX hardware) |
| **Supply chain attack** | ✅ Подменить contract upgrade | ❌ MRENCLAVE изменится → DCAP attestation fail |
| **Rug pull** | ✅ Council выводит всё | ❌ Enclave подпишет withdrawal ТОЛЬКО после margin check |

---

## Что если SGX скомпрометирован?

Теоретические side-channel атаки на SGX существуют (Spectre, Foreshadow). Но:

1. **Один скомпрометированный SGX = один ключ** из трёх. Для вывода нужно 2.
2. **Key rotation:** при обнаружении уязвимости — новые ключи, новый SignerListSet, перевод средств.
3. **Intel microcode updates:** исправляют известные side-channels.
4. **Временное окно:** атакующему нужно скомпрометировать 2 SGX одновременно, до key rotation.

Сравните: в Drift ключ украден один раз — **навсегда**. В нашей архитектуре — даже если один SGX скомпрометирован, у нас есть время на key rotation.

---

## Что если оператор — злоумышленник?

| Действие оператора | Drift | Наша архитектура |
|---|---|---|
| Вывести все средства | ✅ Один tx (admin key) | ❌ Нужно 2-of-3 + enclave подпишет только valid tx |
| Подменить код | ✅ Upgrade contract | ❌ MRENCLAVE изменится → DCAP attestation fail |
| Задержать withdrawals | ✅ Pause contract | ⚠️ Может задержать если он sequencer, но 2 других оператора продолжат |
| Front-run пользователей | ✅ MEV (видит все tx) | ❌ Orders зашифрованы для enclave |
| Подделать цены | ✅ Modify oracle | ⚠️ Медиана от 3 операторов, один не может повлиять |

---

## Практические рекомендации

### Для пользователей нашего DEX:

1. **Проверьте attestation** перед депозитом: `POST /v1/attestation/quote` → верифицируйте MRENCLAVE
2. **Проверьте SignerListSet** на XRPL: убедитесь что escrow имеет quorum=2, master disabled
3. **Убедитесь что операторы на разных провайдерах** (Azure, OVH, Hetzner)
4. **Следите за key rotation** — если MRENCLAVE изменился, проверьте почему

### Для операторов:

1. **Никогда не храните ключи вне SGX** — все ключи генерируются внутри enclave
2. **Disable master key** на escrow account — всегда
3. **Мониторинг:** alerting на нетипичные withdrawals, spending limit guardrails
4. **Регулярный key rotation** — не ждите инцидента
5. **DCAP attestation** — публикуйте MRENCLAVE, дайте пользователям верифицировать

---

## Итог

| | Drift (реальная атака, $280M) | Наша архитектура |
|---|---|---|
| Модель безопасности | Multisig 2-of-5 (люди) | TEE Multisig 2-of-3 (SGX hardware) |
| Вектор атаки | Social engineering 2 людей | Невозможно — hardware не подвержен social engineering |
| Подготовка | Durable nonces за неделю | Enclave не хранит pre-signed tx |
| Минимум для кражи | Убедить 2 человек | Скомпрометировать 2 SGX на разных серверах |
| Время на реакцию | ~1 минута (между legit op и drain) | Есть (key rotation, 2-of-3 продолжает работать) |
| Верификация кода | Audit отчёт (статический) | DCAP attestation (runtime, Intel-signed) |
| Средства | В smart contract (Council control) | На XRPL L1 (SignerListSet, master disabled) |
| Recovery | $280M ушли, протокол на грани смерти | Key rotation + новый escrow + перевод средств |

**$280M Drift hack невозможен в TEE + Multisig архитектуре.**
Не потому что мы умнее — а потому что **подписанты = hardware, а не люди**. Social engineering не работает на процессоры.
