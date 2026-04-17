# Issues from post hackathon.

1.) Perp contract specs do not include a minimum_order_size, amount_step, or price_step

This should be implemented such that get markets returns the contract spec for the market.

2.) The current position abstracts is effectively an islated margin setup. This means that every single position is margined independently, and the margin requirements are calculated on a per-position basis. This is not how most perp exchanges work, and it is not how we want to operate in the long term. We need to implement a cross-margin system where the margin requirements are calculated on a portfolio basis, and the margin is shared across all positions.

This will require a significant redesign of the position and margin management system, and it will also require changes to the way that funding payments are calculated and applied. We need to implement a new margin system that allows for cross-margining, and we need to update the funding payment logic to account for the fact that positions can now share margin.

This is known as a portfolio margin system, and it is a critical component of any professional-grade perp exchange. It allows traders to manage their risk more effectively and it allows for more efficient use of capital.

One of the key differences is that in an isolated margin system, each position is treated as a separate entity, and the margin requirements are calculated independently for each position. In a cross-margin system, the margin requirements are calculated based on the overall risk of the portfolio, and the margin is shared across all positions.

Funding is then calculated based on position size and the funding rate each second, and the funding payments are applied in aggregate each time the position is updated. This means that if a trader has multiple positions, the funding payments will be calculated based on the total size of all positions, and the payments will be applied to the overall margin balance.

3) Amm style liqudity provisioning. This needs to be re-designed to be a virtual amm that posts orders to the clob with prices BASED on a curve.

Note, the curve math is not the same as a traditional AMM. We are not actually swapping assets in and out of a pool. Instead, we are using the curve to determine the prices at which we post orders to the CLOB. The pool is virtual in the sense that it does not hold any actual assets, but it provides liquidity by posting orders based on the curve.

The curve should be a constant product curve where the product of the base and quote reserves is constant. The parameters of the curve (the initial reserves) can be set based on the desired liquidity and price range.

Note, the difficulty here is that we need to set the reserves of the curve in such way that the vault has 50-50 exposure to the market, this means that realistically, the vault (Which is collateralised by XRP) should initially try to post orders to bring it to a delat 0.5 xrp exposure, and then as the market moves, the curve will adjust the prices of the orders it posts to maintain that exposure. This is a slightly different from a traditional AMM where the reserves are fixed and the price is determined by the ratio of the reserves. In our case, we need to dynamically adjust the reserves based on the market price and our desired exposure.

We require 2 types of order labels.
entry orders: these are the orders that the vault posts to the CLOB to provide liquidity. They are based on the curve and they adjust as the market moves to maintain our desired exposure.
exit orders: these are the orders that the vault posts to the CLOB as its entry orders get filled, they post equivalent exit orders to try to close the position and maintain the desired exposure. For example, if an entry order gets filled on the short side, we would post an exit order on the long side to try to close that position.

Example Scenarios:

100k USD of XRPL vault collateral. We initially have 0 exposure to the market. We post sell buy and sell orders on the CLOB based on the curve. As the market moves, the curve will adjust the prices of our orders to maintain that 50k short exposure. If the market goes up, our short position will lose value, but we will be posting higher sell orders to try to maintain that exposure. If the market goes down, our short position will gain value, and we will be posting lower sell orders.

As our entry orders get filled, we will post exit orders to try to close those positions and maintain our desired exposure. For example, if one of our sell orders gets filled, we will post a buy order to try to close that position, this causes another dynamic where we are now posting buy orders that are based on the curve and our desired exposure, so if the market goes down, our buy orders will be posted at lower prices to try to maintain our exposure.


We need tight targets on allowed exposure and collateral utilisation, i.e. we are happy to minimum delta -2 and maximum delta +2 and maximum collateral utilisation of 80%. This means that we want to maintain our exposure within a certain range and we want to make sure that we are not over-utilising our collateral. If we hit those limits.


Withdrawals, we aim to target 80% collateral utilisation, so if we have 100k in the vault, we want to make sure that we are not using more than 80k of that collateral to post orders and maintain our exposure. If we hit that limit, we would need to stop posting new orders and potentially start closing existing positions to reduce our exposure and free up collateral.



