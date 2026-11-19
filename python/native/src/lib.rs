//! Python bindings for hft-sim, imported as `hftsim._native`. Build them from `python/` with
//! `maturin develop --release` inside a virtual environment.
//!
//! `run(args, tables=False)` runs one market from the same flags as the `hftsim` binary, through
//! the same `runner` library, so it reproduces the binary's digest. The simulation runs with the
//! GIL released, so Python threads can run several markets at once. With `tables=True` the result
//! also holds the event log, the top of book and the participants as Arrow-layout buffers, which
//! `hftsim.arrow` turns into pyarrow tables without visiting individual values.
//!
//! No hand-written `unsafe` here; PyO3's macros generate the FFI glue.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use runner::export::{Column, Values};

/// One column as a dict: name, type ("int64", "dictionary" or "bool"), length, values (bytes,
/// little-endian), validity (bytes or None) and names (the dictionary, or None).
fn column<'py>(py: Python<'py>, c: &Column) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("name", c.name)?;
    d.set_item("length", c.len)?;
    let (kind, names) = match &c.values {
        Values::I64(_) => ("int64", None),
        Values::Codes(_, n) => ("dictionary", Some(*n)),
        Values::Bool(_) => ("bool", None),
    };
    d.set_item("type", kind)?;
    d.set_item("values", PyBytes::new(py, &c.value_bytes()))?;
    match &c.validity {
        Some(bits) => d.set_item("validity", PyBytes::new(py, bits))?,
        None => d.set_item("validity", py.None())?,
    }
    match names {
        Some(n) => d.set_item("names", PyList::new(py, n.iter())?)?,
        None => d.set_item("names", py.None())?,
    }
    Ok(d)
}

fn columns<'py>(py: Python<'py>, cols: &[Column]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for c in cols {
        list.append(column(py, c)?)?;
    }
    Ok(list)
}

/// run(args, tables=False) -> (report_json, tables_or_None)
///
/// Run one market from `hftsim run` flags (a list of strings; `--out` is ignored). Returns the
/// JSON report as text and, if `tables` is true, a dict with "events" and "book" (lists of column
/// dicts) and "owners" (a list of (id, role, label) tuples). Invalid flags raise ValueError.
#[pyfunction]
#[pyo3(signature = (args, tables = false))]
fn run(py: Python<'_>, args: Vec<String>, tables: bool) -> PyResult<(String, PyObject)> {
    let result = py.allow_threads(move || runner::simulate(&args, tables)).map_err(PyValueError::new_err)?;
    let out = match &result.tables {
        None => py.None(),
        Some(t) => {
            let d = PyDict::new(py);
            d.set_item("events", columns(py, &t.events)?)?;
            d.set_item("book", columns(py, &t.book)?)?;
            let owners = PyList::empty(py);
            for (id, role, label) in &t.owners {
                owners.append((*id, *role, label.as_str()))?;
            }
            d.set_item("owners", owners)?;
            d.into_any().unbind()
        }
    };
    Ok((result.json, out))
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add("USAGE", runner::USAGE)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
