//! `hftsim`: run one simulated market from command-line flags and print every phase-4 metric as
//! JSON, with a manifest that reproduces the run; write a simulated market as LOBSTER files; and
//! rebuild a LOBSTER stock-day, checking it against its own snapshots. The work is in the `runner`
//! and `data` libraries, which the Python bindings call too.
//!
//! ```text
//! hftsim run --seed 2026 --secs 300 --mm 7 --informed 10 --background zi
//! hftsim lobster-export out/ --levels 10 --secs 60 --mm 7
//! hftsim lobster-check AAPL_2012-06-21_34200000_57600000_message_10.csv AAPL_2012-06-21_34200000_57600000_orderbook_10.csv
//! hftsim help
//! ```

#![forbid(unsafe_code)]

use std::path::Path;

const COMMANDS: &str = "hftsim run [flags]                                     one market -> JSON report (flags below)
hftsim lobster-export DIR [--levels N] [run flags]     one market -> LOBSTER message and order-book files in DIR
hftsim lobster-check MESSAGE ORDERBOOK [--levels N]    rebuild a LOBSTER day and check it against its snapshots
hftsim lobster-check DIRECTORY                         the same for every message/order-book pair found under DIRECTORY
hftsim l3-check EVENTS SNAPSHOTS [--depth 100] [...]   rebuild an order-level feed (e.g. a Bitstamp recording,
                                                       python -m hftsim.bitstamp) and check it against its snapshots
    l3-check flags: --tolerance-us 0 (offset search)  --event-stamp-us 1 (event stamp unit; Bitstamp 1000)
hftsim replay-check [probe flags] [run flags]          replay fill model against the exchange's own fills
hftsim replay-truth MESSAGE_FILE [--from-seconds S]    replay fill model against real orders in a LOBSTER message
                                                       file, leave-one-out (e.g. python -m hftsim.bitstamp replay)
    probe flags: --probes 4  --probe-interval-ms 500  --probe-life-ms 5000  --probe-lots 1
                 --probe-latency colocated|proximity|remote";

fn l3_check(args: &[String]) -> Result<String, String> {
    use data::provenance::{DataLevel, Provenance, file_info};
    let usage = "usage: hftsim l3-check EVENTS_CSV SNAPSHOTS_CSV [--depth 100] [--tolerance-us 0] [--event-stamp-us 1] [--source TEXT] [--symbol S] [--date D] [--also FILE ...]";
    let (events_path, snaps_path) = match args {
        [e, s, ..] => (e, s),
        _ => return Err(usage.into()),
    };
    let (mut depth, mut source, mut symbol, mut date, mut also) = (100usize, "order-level events and snapshots".to_string(), "unknown".to_string(), "unknown".to_string(), Vec::new());
    let (mut tolerance_us, mut stamp_us) = (0i64, 1i64);
    let mut k = 2;
    while k < args.len() {
        let val = args.get(k + 1).ok_or_else(|| format!("{} needs a value\n{usage}", args[k]))?;
        match args[k].as_str() {
            "--depth" => depth = val.parse().map_err(|e| format!("--depth: {e}"))?,
            "--tolerance-us" => tolerance_us = val.parse().map_err(|e| format!("--tolerance-us: {e}"))?,
            "--event-stamp-us" => stamp_us = val.parse().map_err(|e| format!("--event-stamp-us: {e}"))?,
            "--source" => source = val.clone(),
            "--symbol" => symbol = val.clone(),
            "--date" => date = val.clone(),
            "--also" => also.push(val.clone()),
            other => return Err(format!("unknown flag `{other}`\n{usage}")),
        }
        k += 2;
    }
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}"));
    let events = data::l3::parse_events(&read(events_path)?).map_err(|e| format!("{events_path}: {e}"))?;
    let snaps = data::l3::parse_snapshots(&read(snaps_path)?).map_err(|e| format!("{snaps_path}: {e}"))?;
    if stamp_us < 1 {
        return Err("--event-stamp-us must be at least 1".into());
    }
    let v = data::l3::validate_l3(&events, &snaps, depth, tolerance_us, stamp_us);
    let files = [events_path, snaps_path].into_iter().chain(also.iter()).map(|p| file_info(Path::new(p))).collect::<std::io::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    let p = Provenance {
        source,
        symbol,
        date,
        data_level: DataLevel::Mbo,
        levels: depth,
        files,
        notes: vec![
            format!("Order-level snapshots of the best {depth} orders per side; differences outside that range are not seen."),
            "The recording starts with a book already full of orders; those are revealed by snapshots (explained differences).".into(),
        ],
    };
    Ok(format!("{{\"provenance\":{},\"validation\":{}}}", p.to_json(), v.to_json()))
}

fn replay_truth(args: &[String]) -> Result<String, String> {
    use data::provenance::{DataLevel, Provenance, file_info};
    let usage = "usage: hftsim replay-truth MESSAGE_FILE [--from-seconds S] [--takers FILE --fills FILE] [--source TEXT] [--symbol S] [--date D] [--also FILE ...]";
    let (path, rest) = args.split_first().ok_or(usage)?;
    let (mut from_ns, mut source, mut symbol, mut date, mut also) = (0i64, "LOBSTER-format message file".to_string(), "unknown".to_string(), "unknown".to_string(), Vec::new());
    let (mut takers_path, mut fills_path): (Option<String>, Option<String>) = (None, None);
    let mut k = 0;
    while k < rest.len() {
        let val = rest.get(k + 1).ok_or_else(|| format!("{} needs a value\n{usage}", rest[k]))?;
        match rest[k].as_str() {
            "--from-seconds" => from_ns = data::lobster::parse_time_ns(val).ok_or_else(|| format!("--from-seconds: `{val}` is not seconds after midnight"))?,
            "--takers" => takers_path = Some(val.clone()),
            "--fills" => fills_path = Some(val.clone()),
            "--source" => source = val.clone(),
            "--symbol" => symbol = val.clone(),
            "--date" => date = val.clone(),
            "--also" => also.push(val.clone()),
            other => return Err(format!("unknown flag `{other}`\n{usage}")),
        }
        k += 2;
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let messages = data::lobster::parse_messages(&text).map_err(|e| format!("{path}: {e}"))?;
    let v = data::replay::leave_one_out(&messages, from_ns);
    // With the exporter's taker files: the taker-remainder rule and the counterfactual stream.
    let variants = match (&takers_path, &fills_path) {
        (Some(t), Some(f)) => {
            let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}"));
            let info = data::replay::parse_taker_info(&read(t)?, &read(f)?)?;
            let executions = messages.iter().filter(|m| m.kind == data::lobster::Kind::ExecuteVisible).count();
            if info.fill_takers.len() != executions {
                return Err(format!("{f}: {} fills for {executions} executions in {path}", info.fill_takers.len()));
            }
            let run = |taker_rule, counterfactual| data::replay::leave_one_out_with(&messages, from_ns, &data::replay::LooOptions { takers: Some(&info), taker_rule, counterfactual }).to_json();
            also.extend([t.clone(), f.clone()]);
            format!(",\"variants\":{{\"taker_rule\":{},\"counterfactual\":{},\"counterfactual_taker_rule\":{}}}", run(true, false), run(false, true), run(true, true))
        }
        (None, None) => String::new(),
        _ => return Err(format!("--takers and --fills go together\n{usage}")),
    };
    let files = std::iter::once(path).chain(also.iter()).map(|p| file_info(Path::new(p))).collect::<std::io::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    let p = Provenance {
        source,
        symbol,
        date,
        data_level: DataLevel::Mbo,
        levels: 0,
        files,
        notes: vec![
            "Each order submitted in the file is replayed as a copy through the file without its own messages, and the copy's modelled fills are compared with the order's real executions.".into(),
            "Real fills of a resting order are only those the file shows as executions of it; a stream without the order cannot show volume that only it absorbed (the model's blind spot).".into(),
        ],
    };
    Ok(format!("{{\"provenance\":{},\"replay\":{}{variants}}}", p.to_json(), v.to_json()))
}

fn replay_check(args: &[String]) -> Result<String, String> {
    let mut p = runner::ProbeConfig::default();
    let (mut flags, mut k) = (Vec::new(), 0);
    while k < args.len() {
        let key = args[k].as_str();
        if !key.starts_with("--probe") {
            flags.push(args[k].clone());
            k += 1;
            continue;
        }
        let val = args.get(k + 1).ok_or_else(|| format!("{key} needs a value"))?;
        let bad = |e: &dyn std::fmt::Display| format!("{key}: {e}");
        match key {
            "--probes" => p.agents = val.parse().map_err(|e| bad(&e))?,
            "--probe-interval-ms" => p.interval_ms = val.parse().map_err(|e| bad(&e))?,
            "--probe-life-ms" => p.life_ms = val.parse().map_err(|e| bad(&e))?,
            "--probe-lots" => p.lots = val.parse().map_err(|e| bad(&e))?,
            "--probe-latency" => {
                p.location = match val.as_str() {
                    "colocated" => 0,
                    "proximity" => 1,
                    "remote" => 2,
                    other => return Err(format!("unknown latency location `{other}`")),
                }
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
        k += 2;
    }
    Ok(runner::replay_check(&flags, &p)?.to_json())
}

fn lobster_export(args: &[String]) -> Result<String, String> {
    let (dir, rest) = args.split_first().ok_or("usage: hftsim lobster-export DIR [--levels N] [run flags]")?;
    let (mut levels, mut flags, mut k) = (10usize, Vec::new(), 0);
    while k < rest.len() {
        if rest[k] == "--levels" {
            levels = rest.get(k + 1).ok_or("--levels needs a value")?.parse().map_err(|e| format!("--levels: {e}"))?;
            k += 2;
        } else {
            flags.push(rest[k].clone());
            k += 1;
        }
    }
    if levels == 0 {
        return Err("--levels must be at least 1".into());
    }
    runner::lobster_export_dir(&flags, levels, Path::new(dir))
}

fn lobster_check(args: &[String]) -> Result<String, String> {
    let usage = "usage: hftsim lobster-check MESSAGE_FILE ORDERBOOK_FILE [--levels N]  |  hftsim lobster-check DIRECTORY";
    if let [dir] = args {
        if Path::new(dir).is_dir() {
            return data::lobster::validate_dir(Path::new(dir));
        }
    }
    let (messages, book) = match args {
        [m, b, ..] => (m, b),
        _ => return Err(usage.into()),
    };
    let levels = match &args[2..] {
        [] => None,
        [flag, n] if flag == "--levels" => Some(n.parse::<usize>().map_err(|e| format!("--levels: {e}"))?),
        _ => return Err(usage.into()),
    };
    let (v, p) = data::lobster::validate_files(Path::new(messages), Path::new(book), levels)?;
    Ok(format!("{{\"provenance\":{},\"validation\":{}}}", p.to_json(), v.to_json()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => runner::run(&args[1..]),
        Some("lobster-export") => lobster_export(&args[1..]),
        Some("lobster-check") => lobster_check(&args[1..]),
        Some("replay-check") => replay_check(&args[1..]),
        Some("replay-truth") => replay_truth(&args[1..]),
        Some("l3-check") => l3_check(&args[1..]),
        _ => {
            println!("{COMMANDS}\n\n{}", runner::USAGE);
            return;
        }
    };
    match result {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Err(e) => {
            eprintln!("hftsim: {e}");
            std::process::exit(2);
        }
    }
}
