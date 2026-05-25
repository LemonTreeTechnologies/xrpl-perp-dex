# Vault Design 

The purpose of this document is to outline the design of the liquidity vaults.

These vaults are designed to offer a number of mechanisms by which users can deposit their assets and earn yield, while also enabling the protocol to place orders onto the book.

We have designed several vaults with different risk profiles and we will add more as the project evolves.


NOTE: Protocol Vaults are special actors in the ecosystem such that they receive a rebate on orders that are executed against their orders. This means that they can earn a spread on the orders they place, as well as earn fees from the trades that are executed against their orders. This is a key feature of the vaults and is a component of the yield that users can earn by depositing their assets into the vaults. The rebate is a percentage of the fees that are paid by the taker when they execute a trade against the vault's orders. This rebate is designed to incentivize the vaults to provide liquidity to the order book and to earn yield for their users.

Revenue Streams for Vaults:

- Spread: The difference between the buy and sell prices of the orders placed by the vaults. The vaults earn a spread on the orders they place, which is a key component of the yield that users can earn by depositing their assets into the vaults.

- Fee rebate: The rebate that the vaults receive on orders that are executed against their orders. This rebate is a percentage of the fees that are paid by the taker when they execute a trade against the vault's orders. This rebate is designed to incentivize the vaults to provide liquidity to the order book and to earn yield for their users.

- Yield from other strategies: The vaults may also earn yield from other strategies, such as lending or staking, depending on the assets that are deposited into the vaults and the strategies that are implemented by the vaults.

- Funding Rate: The vaults may also earn yield from funding rates, which are payments made by one side of a perpetual contract to the other side, based on the difference between the perpetual contract price and the spot price of the underlying asset. The vaults can earn yield from funding rates by taking positions in perpetual contracts and earning payments from the funding rates.

## Accepted Liquidity.

The vaults accept the following liqudity:

1) XRP: The native asset of the XRP Ledger. It is used for transaction fees and is the native currency of the ledger.

## TODO: Add more assets as we expand the vault offerings.

## Vault Types

### 1. Market Making Vault

This vault allows users to deposit their XRP and earn yield by providing liquidity to the order book. The vault will place orders on the book and earn a spread between the buy and sell prices. The vault will also earn fees from the trades that are executed.

The Order size and price is based on placing orders at a certain spread from the mid price, and the size of the orders is based on a percentage of the vault's total assets. The vault will also rebalance its orders on the book at a certain frequency to maintain its desired spread and order size parameters.

The strategy for this vault is designed to be low risk and contains the following parameters:

- Min Spread: The minimum spread that the vault will maintain between the buy and sell orders. This is designed to ensure that the vault earns a spread on the orders it places.

- Max Spread: The maximum spread that the vault will maintain between the buy and sell orders. This is designed to ensure that the vault does not place orders that are too far apart, which could result in missed trading opportunities.

- Order Size as a percentage of the vault's total assets: This parameter determines the size of the orders that the vault will place on the book. This is designed to ensure that the vault does not place orders that are too large, which could result in significant losses if the market moves against the vault's orders.

- Rebalance Frequency: This parameter determines how often the vault will rebalance its orders on the book. This is designed to ensure that the vault maintains its desired spread and order size parameters.

- Max Delta: This parameter determines the maximum delta that the vault can pick up as a result of its orders being executed. This is designed to ensure that the vault does not take on too much risk by picking up large deltas from executed orders.

- Min Delta: This parameter determines the minimum delta that the vault can pick up as a result of its orders being executed. This is designed to ensure that the vault does not take on too little risk by picking up small deltas from executed orders.

### 2. Delta Neutral Vault
This vault is primarily a short strategy vault that allows users to deposit their XRP and earn yield by providing liquidity to the order book while maintaining a delta neutral position. The vault will place orders on the book and earn a spread between the buy and sell prices, while also earning fees from the trades that are executed. The vault will aim to take positions in perpetual contracts to maintain a delta neutral position, which means that the vault's overall exposure to the underlying asset's price movements is minimized, and enables the vault to earn yield from both the spread and the funding rates.

The strategy for this vault is designed to be medium risk and contains the following parameters:

- Min Spread: The minimum spread that the vault will maintain between the buy and sell orders. This is designed to ensure that the vault earns a spread on the orders it places.

- Max Spread: The maximum spread that the vault will maintain between the buy and sell orders. This is designed to ensure that the vault does not place orders that are too far apart, which could result in missed trading opportunities.

- Order Size as a percentage of the vault's total assets: This parameter determines the size of the orders that the vault will place on the book. This is designed to ensure that the vault does not place orders that are too large, which could result in significant losses if the market moves against the vault's orders.

- Rebalance Frequency: This parameter determines how often the vault will rebalance its orders on the book. This is designed to ensure that the vault maintains its desired spread and order size parameters.

- Max Delta: This parameter determines the maximum delta that the vault can pick up as a result of its orders being executed. This is designed to ensure that the vault does not take on too much risk by picking up large deltas from executed orders.

- Min Delta: This parameter determines the minimum delta that the vault can pick up as a result of its orders being executed. This is designed to ensure that the vault does not take on too little risk by picking up small deltas from executed orders.


### 4. Delta One Vault

This is a vault which benefits from the interest rate discrepency between borrowing USD and the funding rate on the perpetuals. The vault will place orders on the book and earn a spread between the buy and sell prices, while also earning fees from the trades that are executed. The vault will aim to take positions in perpetual contracts to maintain a delta one position, which means that the vault's overall exposure to the underlying asset's price movements is maintained, and enables the vault to earn yield from both the spread and the funding rates.

The flow of this vault is as follows;

1) The User deposits XRP into the vault.
2) The vault uses this collateral to borrow USD from the lending protocol.
3) The vault then uses the borrowed USD to buy spot XRP on the exchange.
4) The vault then shorts the perpetual contract on the exchange, which allows it to earn the funding rate while maintaining a delta one position.

Note, this particular vault is designed to be higher risk than the other vaults, as it involves borrowing and taking on leverage, which can amplify losses if the market moves against the vault's positions. However, it also has the potential to earn higher yields due to the interest rate discrepancy between borrowing USD and the funding rate on the perpetuals.

Note, there are several missing pieces to this vault;

1) Spot RLUSD / XRP market on the exchange: This is needed for the vault to be able to buy spot XRP using the borrowed USD.
2) Lending protocol integration: The vault needs to be able to borrow USD using the XRP collateral that is deposited by the users. This requires integration with a lending protocol that supports borrowing USD against XRP collateral.


