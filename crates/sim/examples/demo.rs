//! One simulated second of a small market: 30 zero-intelligence traders, a third each with
//! co-located, proximity and remote latency. Prints what happened and compares the true book with
//! what traders at different distances believe it is at the same instant.
//!
//! ```text
//! cargo run --release -p sim --example demo
//! ```
//!
//! The traders are random (zero intelligence). Differences in their PnL say nothing about
//! strategies; they only show the plumbing.

use lob::{InstrumentSpec, MboBook, Price, Qty};
use sim::agents::ZeroIntelligence;
use sim::{AgentConfig, AgentId, FEE_SCALE, FeeSchedule, InstrumentConfig, LatencyProfile, SimBuilder};

fn top(book: Option<&MboBook>) -> (Option<(Price, Qty)>, Option<(Price, Qty)>) {
    book.map_or((None, None), |b| (b.best_bid(), b.best_ask()))
}

fn main() {
    let mut b = SimBuilder::new(2026);
    let fees = FeeSchedule { maker: -20, taker: 30, auction: 0 };
    let i = b.add_instrument(InstrumentConfig::new(InstrumentSpec::example()).with_fees(fees));
    let mut members: Vec<(AgentId, &str)> = Vec::new();
    for a in 0..30 {
        let (group, latency) = match a % 3 {
            0 => ("co-located", LatencyProfile::colocated()),
            1 => ("proximity", LatencyProfile::proximity()),
            _ => ("remote", LatencyProfile::remote()),
        };
        let id = b.add_agent(ZeroIntelligence::new(i), AgentConfig::new(latency).subscribe(i));
        members.push((id, group));
    }
    let mut sim = b.build().expect("valid market");

    let wall = std::time::Instant::now();
    sim.run_until(1_000_000_000);
    let wall = wall.elapsed();
    let s = *sim.stats();
    println!("1 simulated second in {:.0} ms of wall time", wall.as_secs_f64() * 1e3);
    println!(
        "{} requests, {} engine commands, {} trades, {} reports and {} market-data packets delivered",
        s.requests, s.engine_commands, s.trades, s.reports_delivered, s.feed_packets_delivered
    );
    println!("longest engine queue: {} commands; run digest {:016x}", s.max_engine_queue, sim.digest());

    let truth = (sim.engine(i).best_bid(), sim.engine(i).best_ask());
    println!();
    println!("best bid / ask at t = 1 s (price in cents, quantity):");
    println!("  exchange (true)       {:?} / {:?}", truth.0, truth.1);
    for wanted in ["co-located", "remote"] {
        let id = members.iter().find(|(_, g)| *g == wanted).expect("group member").0;
        let view = top(sim.book_view(id, i));
        let note = if view == truth { "same" } else { "STALE: messages still in flight" };
        println!("  {wanted:<10} trader's view  {:?} / {:?}   ({note})", view.0, view.1);
    }

    let mark = sim.engine(i).reference_price();
    println!();
    println!("per group, marked at the last trade price {mark}:");
    println!("  {:<11} {:>7} {:>10} {:>10} {:>14}", "group", "fills", "maker qty", "taker qty", "net PnL ($)");
    for group in ["co-located", "proximity", "remote"] {
        let (mut fills, mut maker, mut taker, mut pnl) = (0u64, 0i64, 0i64, 0i128);
        for (id, _) in members.iter().filter(|(_, g)| *g == group) {
            let l = sim.ledger(*id, i);
            fills += l.fills;
            maker += l.maker_qty;
            taker += l.taker_qty;
            pnl += l.net_pnl_fee_units(mark);
        }
        // fee units -> price units (cents) -> dollars
        let dollars = pnl as f64 / FEE_SCALE as f64 / 100.0;
        println!("  {group:<11} {fills:>7} {maker:>10} {taker:>10} {dollars:>14.2}");
    }
    println!();
    println!("(zero-intelligence traders: these PnL numbers are noise, not evidence about latency)");
}
