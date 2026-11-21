# Member peers (`--member-peers`)

**Languages:** English | [Русский](member-peers.ru.md)

Each orchestrator can be told which libp2p peers are the cluster's members:

```
--member-peers <peer_id>,<peer_id>,<peer_id>
```

Each operator lists **every** member, its own node included. A node prints its own peer id at startup
(`P2P started … peer_id=12D3Koo…`); the id is derived from the persistent identity key
(`--p2p-key-path`), so it is stable across restarts.

## What it does

- **Election and signing topics:** a message is accepted only when both its signed author and the peer
  that relayed it are members.
- **Always, with or without the list:** an election message is accepted only from the peer it names.
- **Replicated-log topics and the log fetch protocol:** they talk only to members, and the log transport
  refuses to start unless the list is set.
- An absent or empty list leaves election and signing open, as before, and logs a warning.

`--p2p-peers` is unchanged: it is only the list of addresses to dial, not an access rule.

## What it does not do

The list decides who may *speak* on these channels. It does not decide what a message may *do*: batches
and certificates are accepted only with member signatures verified inside the enclave against the sealed
signer list. A wrong list on one node affects that node's connectivity, not the cluster's authority.

## Replicated-log mode (`--log-store-dir`)

When the enclave is in replicated-log mode, a node runs only with all three of: `--member-peers`,
`--log-store-dir <dir>` (this node's durable log store) and a local signer from the signers config.

- **At startup**, before any peer is dialled, the orchestrator asks the enclave whether it is in log mode.
  In log mode with any of the three missing, it refuses to start and names what is missing. A log store
  set without the rest is refused too, before genesis as well (an enclave build without the log
  ignores the setting with a warning). If the enclave's answer cannot be read, startup
  retries for about two minutes and then refuses; it never assumes "not in log mode".
- **While running without the log**, the orchestrator keeps checking. When the enclave enters log mode,
  the process stops (exit code 78), and the next start applies the rule above.
- With all three set, the node follows the log. `/v1/system/status` reports it under `log`: mode,
  enclave and stored position, `halted`, `diverged` and recent alarms. The list must include this node
  itself, or the follower refuses to start.
- **Sync gate.** The follower applies entries only while the node's XRPL endpoint (`--xrpl-url`) reports
  `server_state` `full` or above, at least 3 peers, and a validated ledger younger than five measured close
  intervals plus 5 s (the interval is clamped to 2–10 s). Otherwise it waits; `log.sync` shows why.
- **View compare.** After each entry every member publishes its state digest. If two members agree and one
  differs, the odd one is named: a peer in an alarm, this node by stopping (`diverged: true`). While views
  disagree with no two agreeing, `log.view_hold` is true (the producing node will not commit on top of it).

## Operations

- Adding or removing a member means updating the list on **every** node and restarting each orchestrator.
- A malformed peer id stops startup (the list is never silently shortened).
