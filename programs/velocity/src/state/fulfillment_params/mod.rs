//! Fulfillment parameter types for spot order routing. The remaining venue is
//! the internal Velocity AMM and the market's own book; external venue support
//! (Serum, Phoenix, OpenBook) was removed when spot order-book trading was
//! disabled.

pub mod velocity;
