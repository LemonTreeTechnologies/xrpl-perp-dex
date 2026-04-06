# Questions to be answered:

1. How to actually run the orchestrator?

```bash
# create a config file as so?

cd orachestrator
make run
```

2. Price is got from the enclave endpoint.
- How is the price calculated? It should be an oracle price, is this implemented? 

3. We are using a custom type for the prices, whhich poorly simulates decimal numbers. We should use a better type, such as `rust_decimal` or `bigdecimal`. (rust decimal is much faster).

4. What exactly is the purpose of the orchestrator? Is it to hold the book or just to integrate with the enclave? 
Afaik, the oorders are submitted to the enclave, so where is the exactly the book held? Is it in the enclave or in the orchestrator? If it's in the enclave, then what is the purpose of the orchestrator? Simply to act as an api gateway?

5. TradingEngine does 2 things, it both holds the book and submits state to be syncronised with the enclave. How is the book syncronised with the enclave? Does the enclave send the updated book to the orchestrator after processing an order? Or does the orchestrator request the updated book from the enclave after processing an order?


## Ordering issue and questions.

1. Wen thhis is deployed as market of distributed system, what is the ordering of a sent order. 
- User sends order to orchestrator
- Orchestrator sends order to enclave
- Enclave processes order and updates book?
- Enclave sends updated book to orchestrator?
- Enclave sends updated book to OTHER orchestrators?

What happens if 2 orders are sent at the same time to 2 different orchestrators? How is the ordering of these 2 orders determined? Is it just the order in which they are received by the enclave? What if enclave 1 receives order A first and enclave 2 receives order B first, but order B is actually sent before order A?

How is ordering determined in this system? Is it just the first enclave to receive the order? What if there are multiple enclaves? How is the ordering of orders determined across multiple enclaves?

## Endpoints needed in orchestrator

1. positions/get: give me the positions i have in the market, and the details of those positions (size, entry price, etc)
2. funding/rates/get: give me the current funding rates for the markets
3. funding/payments/get: give me the funding payments i have made/received
4. markets/get: give me the details of the markets (current price, market name, etc)

## Correctness and Verifiability

1. openapi spec should be used to generate the models used, at present this is NOT how it is done, the models and the open api spec are completely seperate. Will cause drift and incompatibilities. We should use the spec in build.rs to generate the models used in the orchestrator, this will ensure that the models are always up to date with the spec and that there are no incompatibilities between the models and the spec.
2. We should have a test suite that tests the correctness of the system, by sending orders to the orchestrator and checking the state of the book after processing those orders. This will ensure that the system is working correctly and that the book is being updated correctly after processing orders.
3. https://github.com/77ph/xrpl-perp-dex/blob/master/DEPLOYMENT.md#what-is-exposed-externally-via-nginx This is massively out of date referencing endpoints which dont exist anymore, and not referencing endpoints which do exist. This should be updated to reflect the current state of the system, and should be kept up to date as the system evolves. This will ensure that users of the system have accurate information about what endpoints are available and how to use them.
4. 2 openapi specs should be maintained, one for the orchestrator and one for the enclave. This will ensure that the models used in the orchestrator and the enclave are always up to date with the spec and that there are no incompatibilities between the models and the spec. This will also ensure that users of the system have accurate information about what endpoints are available and how to use them.


## Dead code.

1. there is a massive amount of dead code in the orchestrator, which is not being used at all. This should be removed to reduce the complexity of the codebase and to make it easier to understand. This will also reduce the attack surface of the system, as there will be less code that could potentially contain vulnerabilities.

To view;

```bash
cd orchestrator
make fmt lint
```

```
warning: field `market` is never read
  --> src/api.rs:49:9
   |
47 | pub struct SubmitOrderRequest {
   |            ------------------ field in this struct
48 |     pub user_id: String,
49 |     pub market: Option<String>,
   |         ^^^^^^
   |
   = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: constant `SEPOLIA_RPC` is never used
  --> src/commitment.rs:15:11
   |
15 | pub const SEPOLIA_RPC: &str = "https://rpc.sepolia.org";
   |           ^^^^^^^^^^^

warning: constant `SEPOLIA_CHAIN_ID` is never used
  --> src/commitment.rs:16:11
   |
16 | pub const SEPOLIA_CHAIN_ID: u64 = 11155111;
   |           ^^^^^^^^^^^^^^^^

warning: struct `StateCommitment` is never constructed
  --> src/commitment.rs:30:12
   |
30 | pub struct StateCommitment {
   |            ^^^^^^^^^^^^^^^

warning: function `compute_state_hashes` is never used
  --> src/commitment.rs:42:8
   |
42 | pub fn compute_state_hashes(balance_json: &str) -> Result<(String, String)> {
   |        ^^^^^^^^^^^^^^^^^^^^

warning: function `sign_commitment` is never used
  --> src/commitment.rs:50:14
   |
50 | pub async fn sign_commitment(
   |              ^^^^^^^^^^^^^^^

warning: function `submit_to_sepolia` is never used
  --> src/commitment.rs:94:14
   |
94 | pub async fn submit_to_sepolia(
   |              ^^^^^^^^^^^^^^^^^

warning: function `query_commitment` is never used
   --> src/commitment.rs:153:14
    |
153 | pub async fn query_commitment(market_id: [u8; 32]) -> Result<Option<StateCommitment>> {
    |              ^^^^^^^^^^^^^^^^

warning: struct `EnclaveAccount` is never constructed
  --> src/enclave_client.rs:15:12
   |
15 | pub struct EnclaveAccount {
   |            ^^^^^^^^^^^^^^

warning: struct `EnclaveSignature` is never constructed
  --> src/enclave_client.rs:26:12
   |
26 | pub struct EnclaveSignature {
   |            ^^^^^^^^^^^^^^^^

warning: struct `EnclaveClient` is never constructed
  --> src/enclave_client.rs:36:12
   |
36 | pub struct EnclaveClient {
   |            ^^^^^^^^^^^^^

warning: associated items `new`, `generate_account`, `sign_hash`, `pool_status`, and `is_available` are never used
   --> src/enclave_client.rs:43:12
    |
 41 | impl EnclaveClient {
    | ------------------ associated items in this implementation
 42 |     /// Create a new client. TLS verification is disabled (self-signed enclave cert).
 43 |     pub fn new(base_url: &str) -> Result<Self> {
    |            ^^^
...
 56 |     pub async fn generate_account(&self) -> Result<EnclaveAccount> {
    |                  ^^^^^^^^^^^^^^^^
...
 92 |     pub async fn sign_hash(
    |                  ^^^^^^^^^
...
136 |     pub async fn pool_status(&self) -> Result<serde_json::Value> {
    |                  ^^^^^^^^^^^
...
150 |     pub async fn is_available(&self) -> bool {
    |                  ^^^^^^^^^^^^

warning: method `spread` is never used
   --> src/orderbook.rs:164:12
    |
132 | impl OrderBook {
    | -------------- method in this implementation
...
164 |     pub fn spread(&self) -> Option<FP8> {
    |            ^^^^^^

warning: methods `deposit_xrp`, `withdraw`, and `close_position` are never used
   --> src/perp_client.rs:56:18
    |
 15 | impl PerpClient {
    | --------------- methods in this implementation
...
 56 |     pub async fn deposit_xrp(
    |                  ^^^^^^^^^^^
...
 74 |     pub async fn withdraw(
    |                  ^^^^^^^^
...
126 |     pub async fn close_position(
    |                  ^^^^^^^^^^^^^^

warning: fields `trade`, `maker_error`, and `taker_error` are never read
  --> src/trading.rs:38:9
   |
37 | pub struct FailedFill {
   |            ---------- fields in this struct
38 |     pub trade: Trade,
   |         ^^^^^
39 |     pub maker_error: Option<String>,
   |         ^^^^^^^^^^^
40 |     pub taker_error: Option<String>,
   |         ^^^^^^^^^^^
   |
   = note: `FailedFill` has a derived impl for the trait `Debug`, but this is intentionally ignored during dead code analysis

warning: associated items `ONE`, `SCALE`, `to_f64`, and `abs` are never used
  --> src/types.rs:23:15
   |
21 | impl FP8 {
   | -------- associated items in this implementation
22 |     pub const ZERO: FP8 = FP8(0);
23 |     pub const ONE: FP8 = FP8(FP8_SCALE);
   |               ^^^
24 |     pub const SCALE: i64 = FP8_SCALE;
   |               ^^^^^
...
32 |     pub fn to_f64(self) -> f64 {
   |            ^^^^^^
...
42 |     pub fn abs(self) -> Self {
   |            ^^^

warning: enum `PositionStatus` is never used
   --> src/types.rs:169:10
    |
169 | pub enum PositionStatus {
    |          ^^^^^^^^^^^^^^

warning: struct `Position` is never constructed
   --> src/types.rs:188:12
    |
188 | pub struct Position {
    |            ^^^^^^^^

warning: struct `Balance` is never constructed
   --> src/types.rs:200:12
    |
200 | pub struct Balance {
    |            ^^^^^^^

warning: struct `UserBalance` is never constructed
   --> src/types.rs:207:12
    |
207 | pub struct UserBalance {
    |            ^^^^^^^^^^^

warning: function `compress_pubkey` is never used
  --> src/xrpl_signer.rs:21:8
   |
21 | pub fn compress_pubkey(uncompressed: &[u8]) -> Result<Vec<u8>> {
   |        ^^^^^^^^^^^^^^^

warning: function `pubkey_to_xrpl_address` is never used
  --> src/xrpl_signer.rs:48:8
   |
48 | pub fn pubkey_to_xrpl_address(uncompressed_hex: &str) -> Result<String> {
   |        ^^^^^^^^^^^^^^^^^^^^^^

warning: function `der_encode_signature` is never used
  --> src/xrpl_signer.rs:93:8
   |
93 | pub fn der_encode_signature(r: &[u8], s: &[u8]) -> Vec<u8> {
   |        ^^^^^^^^^^^^^^^^^^^^

warning: function `sha512_half` is never used
   --> src/xrpl_signer.rs:127:8
    |
127 | pub fn sha512_half(data: &[u8]) -> [u8; 32] {
    |        ^^^^^^^^^^^

warning: struct `XrplSigner` is never constructed
   --> src/xrpl_signer.rs:138:12
    |
138 | pub struct XrplSigner {
    |            ^^^^^^^^^^

warning: associated items `new`, `sign_xrpl_tx`, `address`, `signing_pubkey`, and `to_account_data` are never used
   --> src/xrpl_signer.rs:154:12
    |
152 | impl XrplSigner {
    | --------------- associated items in this implementation
153 |     /// Create a signer from an enclave client and generated account.
154 |     pub fn new(enclave: EnclaveClient, account: &EnclaveAccount) -> Result<Self> {
    |            ^^^
...
186 |     pub async fn sign_xrpl_tx(
    |                  ^^^^^^^^^^^^
...
222 |     pub fn address(&self) -> &str {
    |            ^^^^^^^
...
227 |     pub fn signing_pubkey(&self) -> &str {
    |            ^^^^^^^^^^^^^^
...
232 |     pub fn to_account_data(&self) -> serde_json::Value {
    |            ^^^^^^^^^^^^^^^
```


## Follow up Questions.

Missing endpoints in DEPLOYMENT.md.

The api router exposes these endpoints for the orchestrator.

```

pub fn router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin("*".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any);

    Router::new()
        .route("/v1/orders", post(submit_order))
        .route("/v1/orders", get(get_orders))
        .route("/v1/orders", delete(cancel_all_orders))
        .route("/v1/orders/{order_id}", delete(cancel_order))
        .route("/v1/account/balance", get(get_balance))
        .route("/v1/markets/{market}/orderbook", get(get_orderbook))
        .route("/v1/markets/{market}/ticker", get(get_ticker))
        .route("/v1/markets/{market}/trades", get(get_trades))
        .route("/v1/openapi.json", get(openapi_spec))
        .route("/v1/attestation/quote", post(attestation_quote))
        .route("/v1/attestation/commitment", get(attestation_commitment))
        .layer(axum::middleware::from_fn(auth::auth_middleware))
        .route("/ws", get(ws::ws_handler))
        .layer(cors)
        .with_state(state)
}
```

The DEPLOYMENT.md only references the following endpoints;

```
| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/v1/perp/balance` | Yes | User balance and positions |
| POST | `/v1/perp/position/open` | Yes | Open position |
| POST | `/v1/perp/position/close` | Yes | Close position |
| POST | `/v1/perp/withdraw` | Yes | Withdraw (margin check + SGX signing) |
| GET | `/v1/perp/liquidations/check` | No | View liquidatable positions |
| GET | `/v1/pool/status` | No | Enclave health |
| POST | `/v1/attestation/quote` | No | DCAP remote attestation |
| WS | `/ws` | No | Real-time trades, orderbook, ticker |
```

Note, all of the following endpoints are missing from the DEPLOYMENT.md;

```
GET | `/v1/orders` | Yes | Get all orders for the user |
DELETE | `/v1/orders` | Yes | Cancel all orders for the user |
DELETE | `/v1/orders/{order_id}` | Yes | Cancel a specific order |
GET | `/v1/account/balance` | Yes | Get user balance |
GET | `/v1/markets/{market}/orderbook` | No | Get orderbook
GET | `/v1/markets/{market}/ticker` | No | Get ticker |
GET | `/v1/markets/{market}/trades` | No | Get recent trades
GET | `/v1/openapi.json` | No | Get OpenAPI spec |
GET | `/v1/attestation/commitment` | No | Get attestation commitment
```

The DEPLOYMENT.md suggests the following endpoints which are not actually implemented;

```
POST | `/v1/perp/position/open` | Yes | Open position |
POST | `/v1/perp/position/close` | Yes | Close position |
POST | `/v1/perp/withdraw` | Yes | Withdraw (margin check + SG
X signing) |
GET | `/v1/perp/liquidations/check` | No | View liquidatable positions
```

Peraps the DEPLOYMENT.md is actually exposing the enclave endpoints, which are not actually exposed by the api router? If so, this should be clarified in the DEPLOYMENT.md, and the enclave endpoints should be added to the DEPLOYMENT.md to reflect the actual state of the system. This will ensure that users of the system have accurate information about what endpoints are available and how to use them.

Additionally, there appears to be a naming mismatch between the endpoints in the DEPLOYMENT.md and the actual endpoints in the api router. For example, the DEPLOYMENT.md references `/v1/perp/position/open` and `/v1/perp/position/close`, but the actual endpoints in the api router are `/v1/orders` with a POST method to open an order, and a DELETE method to close an order. This should be clarified in the DEPLOYMENT.md to reflect the actual state of the system, and to ensure that users of the system have accurate information about how to use the endpoints.
