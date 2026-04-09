# Requirements for Vault

We start with the api requirements for the vault, which will be used by the orchestrator to manage user funds and interact with the XRPL.


## User Api

This is the API that users will interact with to manage their funds in the vault. It includes endpoints for deposits, withdrawals, balance checks, and transaction history.

- `POST /vaults/{vault_id}/deposits`: User deposits funds to vault. Body includes `amount` and `xrpl_tx_hash`. Vault verifies on-chain deposit and credits user balance.
- `POST /vaults/{vault_id}/withdrawals`: User requests withdrawal. Body includes `amount` and `destination_xrpl_address`. Vault checks user balance, creates XRPL transaction, signs with session key, submits to XRPL, returns tx hash.
- `GET /vaults/{vault_id}/balance`: Returns user's current balance in the vault
- `GET /vaults/{vault_id}/transactions`: Returns list of user's deposit and withdrawal transactions with status (pending, confirmed, failed)
- `GET /vaults/{vault_id}/price-per-share`: Returns current price per share for the vault (for share-based accounting)
- `GET /vaults/{vault_id}/price-per-share-history`: Returns historical price per share data for the vault (for charting and analysis)
- `GET /vaults` : Returns list of available vaults with basic info (name, description, current price per share)



## Operator Api
This is the api necessary for the orchestrator to manage the vault and trade on behalf of users. It includes endpoints for checking vault status, managing session keys, and monitoring on-chain activity.

- `GET /vaults/{vault_id}/status`: Returns current status of the vault, including total assets under management, number of users, and recent activity.
- `POST /vaults/{vault_id}/session-key`: Updates the session key used for signing orders. Body includes new session key and old session key. Vault verifies and updates if valid.
- `GET /vaults/{vault_id}/positions`: Returns current open positions held by the vault on the XRPL, including details like size, entry price, and unrealized P&L.
- `GET /vaults/{vault_id}/orders`: Returns list of recent orders placed by the vault on the XRPL, including status (open, filled, cancelled) and details (size, price, side).
- `GET /vaults/{vault_id}/trades`: Returns list of recent trades executed by the vault on the XRPL, including details like size, price, side, and counterparty.
- `POST /vaults/{vault_id}/create-order`: Endpoint for orchestrator to create a new order on the XRPL. Body includes order details (size, price, side). Vault creates and signs order with session key, submits to XRPL, returns order ID and status.


## Admin Api
This is the admin api necessary for creating and managing vaults, as well as performing maintenance tasks.
- `POST /admin/vaults`: Creates a new vault. Body includes vault name, description, and initial session key. Returns new vault ID.
- `DELETE /admin/vaults/{vault_id}`: Deletes a vault. Only allowed if vault has no users and zero balance. Returns success or error.
- `POST /admin/vaults/freeze`: Freezes a vault, preventing new deposits and withdrawals. Body includes vault ID. Returns success or error.
- `POST /admin/vaults/unfreeze`: Unfreezes a vault, allowing deposits and withdrawals again. Body includes vault ID. Returns success or error.


