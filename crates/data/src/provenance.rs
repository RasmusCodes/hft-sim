//! Where a dataset came from. Every result built on historical data carries a [`Provenance`]:
//! the source, the instrument and date, the data level it rests on (ARCHITECTURE §10), and each
//! input file's size and hash, so a result can be traced to the exact bytes it was computed from.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Data level of a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataLevel {
    /// Best bid and offer only.
    L1,
    /// Aggregated price levels.
    L2,
    /// Order by order (market by order).
    Mbo,
}

impl DataLevel {
    /// Name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            DataLevel::L1 => "L1 (best bid and offer)",
            DataLevel::L2 => "L2 (price levels)",
            DataLevel::Mbo => "MBO (order by order)",
        }
    }
}

/// One input file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Path as given.
    pub path: String,
    /// Size in bytes.
    pub bytes: u64,
    /// FNV-1a 64-bit hash of the contents, hex. It detects accidental change, not tampering.
    pub fnv1a64: String,
}

/// FNV-1a 64-bit hash state.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    /// Fold bytes in.
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// Hex digest.
    pub fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Size and hash of a file, read in chunks.
pub fn file_info(path: &Path) -> io::Result<FileInfo> {
    let mut f = File::open(path)?;
    let mut h = Fnv::default();
    let mut buf = vec![0u8; 1 << 16];
    let mut bytes = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok(FileInfo { path: path.display().to_string(), bytes, fnv1a64: h.hex() })
}

/// Origin of a dataset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// Vendor or generator, e.g. "LOBSTER (lobsterdata.com)" or "hft-sim simulation".
    pub source: String,
    /// Instrument.
    pub symbol: String,
    /// Trading date (or the simulation's pseudo-date).
    pub date: String,
    /// Data level the result rests on.
    pub data_level: DataLevel,
    /// Book levels in the snapshots (0 if none).
    pub levels: usize,
    /// Input files.
    pub files: Vec<FileInfo>,
    /// Known limits of the source.
    pub notes: Vec<String>,
}

/// A JSON string literal.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Provenance {
    /// As a JSON object.
    pub fn to_json(&self) -> String {
        let files: Vec<String> = self
            .files
            .iter()
            .map(|f| format!("{{\"path\":{},\"bytes\":{},\"fnv1a64\":{}}}", json_string(&f.path), f.bytes, json_string(&f.fnv1a64)))
            .collect();
        let notes: Vec<String> = self.notes.iter().map(|n| json_string(n)).collect();
        format!(
            "{{\"source\":{},\"symbol\":{},\"date\":{},\"data_level\":{},\"levels\":{},\"files\":[{}],\"notes\":[{}]}}",
            json_string(&self.source),
            json_string(&self.symbol),
            json_string(&self.date),
            json_string(self.data_level.name()),
            self.levels,
            files.join(","),
            notes.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_the_reference_vectors_and_json_is_escaped() {
        // FNV-1a 64 reference values: "" and "a".
        assert_eq!(Fnv::default().hex(), "cbf29ce484222325");
        let mut h = Fnv::default();
        h.update(b"a");
        assert_eq!(h.hex(), "af63dc4c8601ec8c");
        assert_eq!(json_string("C:\\data\\\"x\"\n"), "\"C:\\\\data\\\\\\\"x\\\"\\n\"");
        let p = Provenance { source: "s".into(), symbol: "X".into(), date: "d".into(), data_level: DataLevel::Mbo, levels: 10, files: vec![], notes: vec!["n".into()] };
        assert_eq!(p.to_json(), "{\"source\":\"s\",\"symbol\":\"X\",\"date\":\"d\",\"data_level\":\"MBO (order by order)\",\"levels\":10,\"files\":[],\"notes\":[\"n\"]}");
    }
}
