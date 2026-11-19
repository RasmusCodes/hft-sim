//! Historical market data for hft-sim (phase 5): adapters that normalise vendor files, order-book
//! reconstruction checked against the vendor's own snapshots, provenance for every dataset, and
//! replay fill models. Formats, sources and what has been verified: docs/HISTORICAL_DATA.md.
//!
//! RESULTS ON HISTORICAL DATA ARE A SEPARATE STAGE FROM SIMULATION RESULTS: every result built on
//! this crate carries its [`provenance::Provenance`] and states its data level (L1, L2, MBO).

#![forbid(unsafe_code)]

pub mod itch;
pub mod l3;
pub mod lobster;
pub mod provenance;
pub mod replay;
