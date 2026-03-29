# Perp DEX on XRPL: Feasibility Analysis

**Дата:** 2026-03-29
**Статус:** Исследование
**Дедлайн исследования:** 2026-04-07
**Дедлайн PoC:** 2026-04-15

---

## Резюме

Создание perp DEX **непосредственно на XRPL mainnet невозможно** из-за отсутствия смарт-контрактов. Однако в экосистеме XRPL есть **три жизнеспособных пути**, которые позволяют реализовать perp DEX с использованием RLUSD.

---

## 1. Анализ возможностей XRPL

### Что есть на XRPL mainnet
| Возможность | Статус |
|---|---|
| Встроенный CLOB DEX (спот) | Работает (с момента запуска) |
| AMM (XLS-30) | Работает (с 2024) |
| RLUSD | Работает, market cap >$1.2B |
| Смарт-контракты | **НЕТ** |
| Hooks | **НЕТ** (только на Xahau sidechain) |
| Деривативы | **НЕТ** |

### Ключевые ограничения XRPL mainnet для perp DEX
1. **Нет программируемости** — нельзя реализовать margin engine, liquidation, funding rate
2. **Только спот-торговля** — нативный DEX не поддерживает синтетические активы
3. **Нет оракулов** — нет механизма ценовых фидов
4. **3-5 сек финальность** — медленно для деривативов

### Смежные решения в экосистеме

| Решение | Статус | Смарт-контракты | Возможность perp |
|---|---|---|---|
| XRPL Mainnet | Production | Нет | Невозможно |
| Xahau (Hooks) | Production (Oct 2023) | Ограниченные (WASM, 64KB стек, не Тьюринг-полные) | Крайне сложно |
| XRPL EVM Sidechain | Production (Jun 2025) | Полные (Solidity/EVM) | **Возможно** |
| XLS-101d (Native WASM) | Ранний драфт | Планируется | Нет сроков |

---

## 2. Три жизнеспособных архитектуры

### Вариант A: GMX-style на XRPL EVM Sidechain

**Суть:** Oracle-based perp DEX полностью на EVM sidechain, RLUSD как collateral.

```
User --> XRPL EVM Sidechain
          |-- Vault (RLUSD collateral)
          |-- Position Manager
          |-- Oracle (Chainlink/Pyth)
          |-- Liquidation Keepers
          |-- Funding Rate Engine
```

**Плюсы:**
- Проверенная архитектура (GMX на Arbitrum)
- Полная EVM совместимость — можно форкнуть GMX
- RLUSD доступен через Axelar bridge
- Vertex уже строит деривативы на этой цепи
- XRP как gas token

**Минусы:**
- PoA консенсус (централизация)
- Зависимость от Axelar bridge для RLUSD
- Тонкая ликвидность на старте
- Конкуренция с Vertex

**Сложность PoC:** Средняя (2-3 недели форк GMX)

---

### Вариант B: TEE Coprocessor + XRPL Settlement

**Суть:** Order matching и вычисления в SGX enclave, settlement через XRPL mainnet.

```
Users --> [Encrypted Orders] --> SGX Enclave (TEE)
                                   |-- Order matching
                                   |-- Margin calculation
                                   |-- Funding rate calc
                                   |-- Attestation
                                   v
                              [Matched Trades + Attestation]
                                   v
                              XRPL Mainnet Settlement
                                   |-- Escrow/Payment channels
                                   |-- RLUSD transfers
                                   |-- Position tracking (off-chain DB + attestation)
```

**Плюсы:**
- Использует XRPL mainnet напрямую (ценность для гранта)
- Anti-MEV, anti-frontrunning by design
- Референс: SGX_project уже есть (prediction market MVP)
- Уникальная архитектура — нет конкурентов на XRPL
- RLUSD settlement на L1

**Минусы:**
- Position state хранится off-chain (в TEE + backup)
- Зависимость от Intel SGX hardware
- Сложнее в реализации
- Ограниченная децентрализация (TEE operator = trusted party)

**Сложность PoC:** Высокая (3-4 недели), НО есть референс в SGX_project

---

### Вариант C: Hybrid — TEE Matching + EVM Sidechain Settlement

**Суть:** Лучшее из двух миров: TEE для order matching, EVM sidechain для settlement.

```
Users --> [Encrypted Orders] --> SGX Enclave (TEE)
                                   |-- Order matching
                                   |-- Price computation
                                   |-- Attestation
                                   v
                              XRPL EVM Sidechain
                                   |-- Verify attestation
                                   |-- Vault (RLUSD)
                                   |-- Position state
                                   |-- Liquidation
                                   |-- Funding rate
```

**Плюсы:**
- TEE обеспечивает anti-MEV
- EVM обеспечивает on-chain state и settlement
- RLUSD collateral в смарт-контракте
- Наиболее "грантоспособная" — использует несколько компонентов экосистемы

**Минусы:**
- Максимальная сложность реализации
- Две системы для поддержки

**Сложность PoC:** Очень высокая

---

## 3. Что такое RLUSD и как его использовать

- **Эмитент:** Ripple (через trust company, одобрена NYDFS и DFSA)
- **Обеспечение:** 1:1 USD cash + US Treasuries
- **Цепи:** XRPL mainnet + Ethereum
- **Market cap:** >$1.2B
- **XRPL issuer account:** `rMxCKbEDwqr76QuheSUMdEGf4B9xJ8m5De`
- **Доступность на EVM sidechain:** через Axelar bridge

**Роль RLUSD в perp DEX:**
1. **Collateral** — маржа и обеспечение позиций
2. **Settlement** — расчёты P&L
3. **Insurance fund** — страховой фонд
4. **LP token denomination** — для oracle-based модели

---

## 4. Конкуренты и прецеденты

| Проект | Цепь | Тип | Статус |
|---|---|---|---|
| Vertex | XRPL EVM Sidechain | Derivatives | В разработке |
| Hyperliquid | Custom L1 | Perp CLOB | Production |
| dYdX v4 | Cosmos L1 | Perp CLOB | Production |
| GMX | Arbitrum | Perp Oracle-based | Production |
| XRPL Derivatives Sidechain | Proposal only | Options/Perps | Концепция |

---

## 5. Рекомендация

### Для PoC к 15 апреля: **Вариант B (TEE + XRPL Settlement)**

**Обоснование:**
1. **Уникальность** — нет аналогов на XRPL, дифференциация от Vertex
2. **Грантовая привлекательность** — использует XRPL mainnet + RLUSD напрямую
3. **Референс есть** — SGX_project уже демонстрирует TEE на XRPL
4. **Anti-MEV** — сильный нарратив для грантовой заявки
5. **Масштабируемость** — можно эволюционировать в Вариант C позже

**PoC scope:**
- SGX enclave: простой order matching (limit orders)
- Attestation verification
- RLUSD escrow на XRPL для collateral
- Простой margin check (isolated margin, 1 рынок: XRP/RLUSD perp)
- Funding rate: статический (упрощённый)

### Fallback: **Вариант A (GMX fork на EVM Sidechain)**
Если TEE подход не укладывается в сроки — форк GMX на XRPL EVM Sidechain с RLUSD как collateral. Это технически проще, но менее уникально.

---

## 6. Открытые вопросы для дальнейшего исследования

1. [ ] Liquidity RLUSD на XRPL EVM Sidechain — достаточно ли для DEX?
2. [ ] Есть ли oracle providers (Chainlink/Pyth) на XRPL EVM Sidechain?
3. [ ] Грантовые требования Ripple/XRPL Foundation — конкретные критерии?
4. [ ] SGX_project — что именно реализовано, можно ли переиспользовать?
5. [ ] Юридические ограничения на деривативы в юрисдикции
6. [ ] Payment channels XRPL — можно ли использовать для быстрого settlement?
