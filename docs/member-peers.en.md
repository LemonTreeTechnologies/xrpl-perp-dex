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

## Operations

- Adding or removing a member means updating the list on **every** node and restarting each orchestrator.
- A malformed peer id stops startup (the list is never silently shortened).
