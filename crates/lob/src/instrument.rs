//! Static trading rules of one instrument.

use crate::types::{Price, Qty};

/// Largest absolute price accepted in a spec, so that `price ± tick` arithmetic and
/// level comparisons can never overflow.
const PRICE_LIMIT: Price = 1 << 61;
/// Largest order quantity accepted in a spec, so quantity sums cannot overflow.
const QTY_LIMIT: Qty = 1 << 40;

/// Trading rules of one instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstrumentSpec {
    /// Minimum price increment, in price units. Order prices must be multiples of it.
    pub tick_size: Price,
    /// Minimum quantity increment. Order quantities must be positive multiples of it.
    pub lot_size: Qty,
    /// Lowest valid order price (inclusive, on the tick grid).
    pub min_price: Price,
    /// Highest valid order price (inclusive, on the tick grid).
    pub max_price: Price,
    /// Largest quantity of a single order (a multiple of `lot_size`).
    pub max_order_qty: Qty,
    /// Initial reference price (for example the previous close). It is replaced by the
    /// last trade price and breaks ties in auction price determination.
    pub reference_price: Price,
}

/// Why an [`InstrumentSpec`] is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpecError {
    /// `tick_size` must be positive.
    TickSize,
    /// `lot_size` must be positive.
    LotSize,
    /// Price bounds must be ordered, on the tick grid and within ±2^61.
    PriceBounds,
    /// `max_order_qty` must be a positive multiple of `lot_size` below 2^40.
    MaxOrderQty,
    /// `reference_price` must be on the tick grid and within the price bounds.
    ReferencePrice,
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid instrument spec: {self:?}")
    }
}

impl std::error::Error for SpecError {}

impl InstrumentSpec {
    /// Check internal consistency.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.tick_size <= 0 || self.tick_size > PRICE_LIMIT {
            return Err(SpecError::TickSize);
        }
        if self.lot_size <= 0 || self.lot_size > QTY_LIMIT {
            return Err(SpecError::LotSize);
        }
        let on_grid = |p: Price| p % self.tick_size == 0;
        if self.min_price > self.max_price
            || self.min_price.abs() > PRICE_LIMIT
            || self.max_price.abs() > PRICE_LIMIT
            || !on_grid(self.min_price)
            || !on_grid(self.max_price)
        {
            return Err(SpecError::PriceBounds);
        }
        if self.max_order_qty < self.lot_size
            || self.max_order_qty > QTY_LIMIT
            || self.max_order_qty % self.lot_size != 0
        {
            return Err(SpecError::MaxOrderQty);
        }
        if !on_grid(self.reference_price)
            || self.reference_price < self.min_price
            || self.reference_price > self.max_price
        {
            return Err(SpecError::ReferencePrice);
        }
        Ok(())
    }

    /// Equity-like example: prices in cents, one-cent tick, single-share lots,
    /// reference price $100.00.
    pub fn example() -> Self {
        InstrumentSpec {
            tick_size: 1,
            lot_size: 1,
            min_price: 1,
            max_price: 100_000_000,
            max_order_qty: 1_000_000,
            reference_price: 10_000,
        }
    }
}
