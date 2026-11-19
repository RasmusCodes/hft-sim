//! Exchange-side ground-truth accounting.
//!
//! All money is integer. Cash is in price units × quantity, held in `i128` so notional never
//! overflows (RISKS K13). Fees are in fee units: [`FEE_SCALE`] fee units = 1 price unit per unit
//! of quantity. With prices in cents, a fee of 30 fee units is $0.0003 per share.

use lob::{Liquidity, Price, Qty, Side};

/// Fee units per price unit.
pub const FEE_SCALE: i64 = 10_000;

/// Fees per unit of quantity, in fee units; negative values are rebates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FeeSchedule {
    /// Fee for passive (resting) executions.
    pub maker: i64,
    /// Fee for aggressive executions.
    pub taker: i64,
    /// Fee for auction executions.
    pub auction: i64,
}

impl FeeSchedule {
    /// Fee rate for a liquidity role.
    pub fn rate(&self, liquidity: Liquidity) -> i64 {
        match liquidity {
            Liquidity::Maker => self.maker,
            Liquidity::Taker => self.taker,
            Liquidity::Auction => self.auction,
        }
    }
}

/// One participant's account on one instrument, as the exchange sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ledger {
    /// Net position (bought − sold).
    pub position: Qty,
    /// Cash from trading, price units × quantity (negative after buying).
    pub cash: i128,
    /// Fees paid (negative = rebates earned), in fee units.
    pub fees: i128,
    /// Quantity bought.
    pub bought: Qty,
    /// Quantity sold.
    pub sold: Qty,
    /// Quantity executed passively.
    pub maker_qty: Qty,
    /// Quantity executed aggressively.
    pub taker_qty: Qty,
    /// Quantity executed in auctions.
    pub auction_qty: Qty,
    /// Number of fills.
    pub fills: u64,
}

impl Ledger {
    /// Book one fill.
    pub fn on_fill(&mut self, side: Side, price: Price, qty: Qty, liquidity: Liquidity, fees: &FeeSchedule) {
        let notional = i128::from(price) * i128::from(qty);
        match side {
            Side::Buy => {
                self.position += qty;
                self.cash -= notional;
                self.bought += qty;
            }
            Side::Sell => {
                self.position -= qty;
                self.cash += notional;
                self.sold += qty;
            }
        }
        self.fees += i128::from(fees.rate(liquidity)) * i128::from(qty);
        match liquidity {
            Liquidity::Maker => self.maker_qty += qty,
            Liquidity::Taker => self.taker_qty += qty,
            Liquidity::Auction => self.auction_qty += qty,
        }
        self.fills += 1;
    }

    /// Cash plus inventory valued at `mark`, before fees (price units × quantity).
    pub fn gross_value(&self, mark: Price) -> i128 {
        self.cash + i128::from(self.position) * i128::from(mark)
    }

    /// Net PnL after fees at mark price `mark`, in fee units.
    pub fn net_pnl_fee_units(&self, mark: Price) -> i128 {
        self.gross_value(mark) * i128::from(FEE_SCALE) - self.fees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_fees() {
        let fees = FeeSchedule { maker: -20, taker: 30, auction: 10 };
        let mut l = Ledger::default();
        l.on_fill(Side::Buy, 10_000, 100, Liquidity::Maker, &fees);
        l.on_fill(Side::Sell, 10_002, 100, Liquidity::Taker, &fees);
        assert_eq!(l.position, 0);
        assert_eq!(l.cash, 200);
        assert_eq!(l.fees, -2_000 + 3_000);
        // 2 cents x 100 shares = 200 price units = 2,000,000 fee units, minus 1,000 fees.
        assert_eq!(l.net_pnl_fee_units(10_001), 200 * 10_000 - 1_000);
    }
}
