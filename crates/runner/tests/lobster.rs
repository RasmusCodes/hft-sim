//! Simulated markets written as LOBSTER files and read back (phase 5). The writer and the
//! rebuilder live in `data`; the independent reference is the engine's own public feed rebuilt by
//! `lob::MboBook` (the `book` table of `run_with_tables`).

use std::collections::HashMap;

use data::lobster::{ExportOptions, ExportStats, parse_messages, parse_snapshots, validate_text};
use runner::export::{Column, Values};

fn flags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn field(json: &str, key: &str) -> String {
    let k = json.find(key).expect(key) + key.len();
    json[k..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect()
}

fn export(args: &[String], levels: usize) -> (String, String, String, ExportStats) {
    let (mut msg, mut book) = (Vec::new(), Vec::new());
    let (manifest, stats) = runner::lobster_export(args, &ExportOptions { levels, ..ExportOptions::default() }, &mut msg, &mut book).expect("export");
    (manifest, String::from_utf8(msg).expect("utf-8"), String::from_utf8(book).expect("utf-8"), stats)
}

fn market() -> Vec<String> {
    flags(&["--seed", "11", "--secs", "20", "--warmup-secs", "5", "--mm", "7", "--informed", "5", "--arbs", "2"])
}

#[test]
fn a_simulated_day_round_trips_through_lobster_files() {
    let args = market();
    let (manifest, msg, book, stats) = export(&args, 10);
    let v = validate_text(&msg, &book, 10).expect("the files parse");
    assert!(v.rows > 10_000, "a real stream ({} rows)", v.rows);
    assert_eq!(v.rows, stats.rows);
    assert_eq!((v.matched, v.first_mismatch, v.reconciled_rows, v.unexplained_rows), (v.rows, None, 0, 0), "an empty-book start needs no reconciliation");
    assert_eq!((v.stats.unknown_order_events, v.stats.duplicate_submissions, v.stats.size_mismatches), (0, 0, 0));
    let trades: usize = field(&manifest, "\"trades\":").parse().expect("trades");
    assert_eq!(stats.by_kind[3], trades, "one visible execution per continuous trade");
    // The export is the same run as `hftsim run` with these flags.
    let report = runner::run(&args).expect("run");
    assert_eq!(field(&manifest, "\"digest\":\""), field(&report, "\"digest\":\""));
}

fn ints(c: &Column) -> Vec<Option<i64>> {
    let Values::I64(v) = &c.values else { panic!("{} is not int64", c.name) };
    (0..c.len).map(|k| c.is_valid(k).then_some(v[k])).collect()
}

#[test]
fn the_written_top_of_book_agrees_with_the_engine_feed_handler() {
    let args = market();
    let (_, msg, book, _) = export(&args, 1);
    let (msgs, snaps) = (parse_messages(&msg).expect("messages"), parse_snapshots(&book, 1).expect("snapshots"));
    // The last LOBSTER row at each time (LOBSTER time = 9:30 + simulation time).
    let start = ExportOptions::default().start_ns;
    let last: HashMap<i64, usize> = msgs.iter().enumerate().map(|(k, m)| (m.ts_ns - start, k)).collect();
    let tables = runner::run_with_tables(&args).expect("run").tables.expect("tables");
    let col = |name: &str| ints(tables.book.iter().find(|c| c.name == name).expect(name));
    let (ts, bid, bid_qty, ask, ask_qty) = (col("ts_ns"), col("bid"), col("bid_qty"), col("ask"), col("ask_qty"));
    let mut compared = 0;
    for k in 0..ts.len() {
        let t = ts[k].expect("time");
        let Some(&row) = last.get(&t) else { continue };
        let s = &snaps[row];
        let best = |side: &[(i64, i64)]| side.first().map(|&(p, q)| (p / 100, q));
        assert_eq!(best(&s.bids), bid[k].map(|p| (p, bid_qty[k].expect("size"))), "best bid at {t} ns");
        assert_eq!(best(&s.asks), ask[k].map(|p| (p, ask_qty[k].expect("size"))), "best ask at {t} ns");
        compared += 1;
    }
    assert!(compared > ts.len() * 9 / 10, "nearly every feed-handler time has a LOBSTER row ({compared} of {})", ts.len());
}

#[test]
fn the_replay_queue_model_against_the_exchanges_own_fills() {
    let args = flags(&["--seed", "21", "--secs", "40", "--warmup-secs", "5", "--mm", "7", "--informed", "5"]);
    let probes = runner::ProbeConfig { agents: 6, interval_ms: 300.0, life_ms: 3_000, ..runner::ProbeConfig::default() };
    let r = runner::replay_check(&args, &probes).expect("check");
    println!("replay check: {}", r.to_json());
    assert!(r.probes > 200, "enough probes ({})", r.probes);
    assert_eq!(r.both + r.truth_only + r.model_only + r.neither + r.marketable, r.probes);
    assert_eq!((r.model_only, r.model_earlier), (0, 0), "the conservative model never fills a probe the exchange did not, nor earlier");
    assert_eq!(r.truth_only_visible, 0, "every fill the stream without the probe can show, the model finds");
    // Every fill whose aggressor volume the stream shows is found at the same nanosecond; the
    // rest (the probe took the aggressor's last shares) can only be found later.
    assert!(r.truth_visible > 0 && r.same_time >= r.truth_visible, "visible fills matched exactly: {} of {}", r.same_time, r.truth_visible);
}

#[test]
fn a_directory_of_days_is_found_paired_and_checked() {
    // Two simulated days in nested folders, plus an order-book file without its message file.
    let dir = std::env::temp_dir().join(format!("hftsim-lobster-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    runner::lobster_export_dir(&flags(&["--seed", "1", "--secs", "3", "--mm", "5"]), 3, &dir.join("a")).expect("day a");
    runner::lobster_export_dir(&flags(&["--seed", "2", "--secs", "4", "--mm", "5"]), 5, &dir.join("b").join("nested")).expect("day b");
    std::fs::write(dir.join("ORPHAN_2000-01-01_34200000_34201000_orderbook_1.csv"), "").expect("orphan");
    let found = data::lobster::find_pairs(&dir).expect("readable");
    assert_eq!((found.pairs.len(), found.unpaired.len()), (2, 1));
    let report = data::lobster::validate_dir(&dir).expect("report");
    assert!(report.contains("\"pairs\":2,\"failed\":0,") && report.contains("\"unexplained_rows\":0,\"unpaired_files\":1"), "{}", &report[..report.len().min(400)]);
    let rows: usize = field(&report, "\"rows\":").parse().expect("rows");
    let matched: usize = field(&report, "\"matched\":").parse().expect("matched");
    assert!(rows > 0 && matched == rows, "every row of both days matches ({matched} of {rows})");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_starts_mid_session_is_reconciled_from_its_snapshots() {
    // Cut the first half of the rows: the rest starts with a book full of orders the file never
    // submitted, as a real LOBSTER file does at 9:30.
    let (_, msg, book, _) = export(&market(), 10);
    let (m, b): (Vec<&str>, Vec<&str>) = (msg.lines().collect(), book.lines().collect());
    let cut = m.len() / 2;
    let v = validate_text(&m[cut..].join("\n"), &b[cut..].join("\n"), 10).expect("the files parse");
    println!(
        "mid-session start: {} rows, {} matched unaided ({:.3}%), {} reconciled, seeds +{} -{}, {} events on unknown orders ({} shares), {} unexplained",
        v.rows,
        v.matched,
        100.0 * v.matched as f64 / v.rows as f64,
        v.reconciled_rows,
        v.seed_added,
        v.seed_removed,
        v.stats.unknown_order_events,
        v.stats.unknown_order_volume,
        v.unexplained_rows
    );
    assert_eq!(v.first_mismatch, Some(1), "the first row shows a book the file never built");
    assert_eq!(v.unexplained_rows, 0, "every disagreement is explained by orders that predate the file");
    assert!(v.stats.unknown_order_events > 0);
    assert!(v.matched as f64 >= 0.99 * v.rows as f64, "after the opening, the rebuilt book matches almost every row unaided");
}
