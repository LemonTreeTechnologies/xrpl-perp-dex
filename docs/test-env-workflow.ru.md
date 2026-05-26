# Workflow тестирования по env'ам

**Дата:** 2026-05-26
**Статус:** Активен

В системе три environment'а. Это не параллельные варианты которые выбирает consumer — это последовательные стадии нашего delivery flow.

```
итерация dev-perp     │   pre-prod валидация     │   consumer acceptance
─────────────────────────────────────────────────────────────────────────
Hetzner (single host) │   Azure 3-node cluster   │   Tom + frontend
                      │   (testnet-cluster)      │
                      │                          │
локальный enclave     │   реальный DCAP          │
без multi-op signing  │   multi-operator FROST   │
быстрая итерация      │   failover / promote     │
accumulation-баги     │                          │
видны здесь           │                          │
```

## Поток на каждый feature

1. **dev-perp пишет + preflight на Hetzner.** Code verification, accumulation-class сценарии (state накапливающийся через restart'ы вылазит здесь), быстрая debug-rebuild итерация. Hetzner enclave = single-node dev SGX, без DCAP — баги формы attestation невидимы. Multi-operator signing bypass'ится (используется single-operator путь).
2. **Deploy на Azure testnet-cluster (staging).** Binary копируется на все 3 ноды, systemd swap. Cluster-only поведение валидируется здесь: sequencer failover, multi-operator FROST signing, real DCAP attestation, libp2p mesh динамика. Production-shape поведение появляется здесь — Hetzner by design не покажет.
3. **Tom (и мы вместе с ним) тестируем через `api-dev.xperp.fi`.** Через TLS + Hetzner nginx + passive failover (`proxy_next_upstream` на 503) запрос ложится на текущий Azure sequencer. Tom никогда не должен знать про per-env поведение — для него это **один черный ящик с full system features**. Баги которые Tom surfaces возвращаются на шаг 1 для fix, потом 2, потом 3 (round-trip).

## Почему это закрывает "Hetzner ≠ Azure equivalence"

Каждый env contribute'ит разную surface валидации:

| Surface | Где появляется |
|---|---|
| Vault curve math, ladder, posture transitions | Hetzner + Azure (оба имеют code path) |
| Accumulation-баги (Q-19-4-like) | Hetzner mandatory (state накапливается через restart'ы естественно) |
| Failover / singleton respawn (X-19-2-like) | Azure mandatory (cluster-only) |
| Multi-operator FROST signing | Azure mandatory |
| Real DCAP attestation | Azure mandatory |
| External-consumer access path (TLS, auth, headers) | Tom acceptance через `api-dev.xperp.fi` |

Факт «two envs differ» перестаёт быть синхронизационной миной потому что workflow последовательный, не параллельный. Tom не видит разницу — он видит только черный ящик `api-dev.xperp.fi`. Cross-env discipline живёт у dev-perp.

## Production env (отдельный, ещё не provisioned)

`api.xperp.fi` зарезервирован как production hostname; сейчас temporarily aliased на dev и будет re-pointed когда production VMs provisioned. Production-shape требования (Azure Load Balancer с health-checked dynamic upstream, real DCAP allowlist, codified Terraform / Azure CLI infrastructure) живут в production roadmap, не в V1 vault closure.

## Сегодня (state 2026-05-26)

- `api-dev.xperp.fi` → Hetzner nginx (TLS, Let's Encrypt) → `proxy_next_upstream` через Azure 3-node cluster на `:3000` → current sequencer → DCAP-attested enclave.
- Hetzner orchestrator на `:3003` продолжает работать как наш dev workshop; нет публичного hostname.
- NSG `:3000` открыт только для Hetzner IPv4 `94.130.18.162`.
- Failover verified live: убиваем Azure node-1, signed POST через `api-dev.xperp.fi` succeeds на next sequencer (node-2).
