"""Drive the `hftsim` binary: build scenarios, run them (in parallel, cached), verify manifests.

Every run is a separate process, so a grid of runs uses every core and each run is isolated. The
same seed across scenarios gives common random numbers for paired comparisons.
"""

from __future__ import annotations

import concurrent.futures as cf
import dataclasses
import hashlib
import json
import os
import subprocess
import sys
import threading
from pathlib import Path
from typing import Iterable

REPO = Path(__file__).resolve().parents[2]
CACHE = REPO / "python" / ".cache"
_build_lock = threading.Lock()
_built = False


def binary() -> Path:
    """Path of the release `hftsim` binary (built by `build()`)."""
    return REPO / "target" / "release" / ("hftsim.exe" if os.name == "nt" else "hftsim")


def build() -> None:
    """Build the release binary (offline, no downloads)."""
    subprocess.run(["cargo", "build", "--release", "--offline", "-q", "-p", "runner"], cwd=REPO, check=True)


def ensure_built() -> Path:
    """Build the release binary once per process and return its path. An existing binary is no
    proof it matches the sources, so this always asks cargo, which rebuilds only what changed and
    leaves an up-to-date binary untouched (the run cache, keyed on the binary, stays valid). Without
    cargo, an existing binary is used with a warning."""
    global _built
    with _build_lock:
        if not _built:
            try:
                build()
            except FileNotFoundError:
                if not binary().exists():
                    raise
                print("warning: cargo not found; using the existing hftsim binary, which may be older than the sources", file=sys.stderr)
            _built = True
    return binary()


@dataclasses.dataclass(frozen=True)
class Scenario:
    """One market configuration. Every field maps to an `hftsim run` flag; all flags are passed
    explicitly, so a manifest specifies its run completely even if the binary's defaults change.
    Field defaults equal the binary's (checked by the tests)."""

    secs: int = 300
    warmup_secs: int = 60
    background: str = "default"
    value_ms: int = 1000
    maker_fee: int = 0
    taker_fee: int = 0
    informed: int = 0
    informed_noise: float = 0.5
    informed_threshold: int = 1
    informed_clip: int = 2
    informed_latency: str = "proximity"
    arbs: int = 0
    arb_threshold: int = 2
    arb_clip: int = 5
    arb_latency: str = "colocated"
    makers: tuple[int, ...] = ()
    mm_latency: str = "colocated"
    mm_unit_lots: int = 5
    mm_max_inventory: int = 10
    mm_gamma: float = 3.5
    mm_sigma: float = 1.0
    mm_a: float = 130.0
    mm_k: float = 5.0
    mm_skew: float = 0.1
    mm_half_spread: float = 0.5
    mm_as_horizon: float = 0.03
    horizons_ms: tuple[int, ...] = (50, 1000, 15000, 60000, 300000)
    race_window_us: int = 500
    queue_edges: tuple[int, ...] = (0, 1, 50, 100, 200, 500)
    ofi_ms: tuple[int, ...] = (1000, 10000)

    def args(self, seed: int) -> list[str]:
        """The flags for one seed."""
        out = ["--seed", str(seed)]
        for f in dataclasses.fields(self):
            v = getattr(self, f.name)
            if f.name == "makers":
                for step in v:
                    out += ["--mm", str(step)]
                continue
            flag = "--" + f.name.replace("_", "-")
            out += [flag, ",".join(str(x) for x in v) if isinstance(v, tuple) else repr(v) if isinstance(v, float) else str(v)]
        return out

    def with_(self, **changes) -> "Scenario":
        return dataclasses.replace(self, **changes)


def native():
    """The compiled extension `hftsim._native`, or None if it has not been built
    (`maturin develop --release` in python/)."""
    try:
        from . import _native
    except ImportError:
        return None
    return _native


def _cache_key(args: list[str], use_native: bool = False) -> str:
    if use_native:
        st = Path(native().__file__).stat()
        blob = json.dumps([st.st_mtime_ns, st.st_size, args, "native"]).encode()
    else:
        st = binary().stat()
        blob = json.dumps([st.st_mtime_ns, st.st_size, args]).encode()
    return hashlib.sha256(blob).hexdigest()[:32]


def run_args(args: list[str], cache: bool = True, use_native: bool = False) -> dict:
    """Run `hftsim run` with these flags and return its JSON result. Results are cached by flags
    and build (a rebuild invalidates the cache). With `use_native` the run happens in this process
    through the compiled extension, with the GIL released; the result is the same as the binary's."""
    if use_native and native() is None:
        raise RuntimeError("hftsim._native is not built: run `maturin develop --release` in python/")
    if not use_native:
        ensure_built()
    path = CACHE / f"{_cache_key(args, use_native)}.json"
    if cache and path.exists():
        return json.loads(path.read_text(encoding="utf-8"))
    if use_native:
        try:
            text, _ = native().run(list(args))
        except ValueError as e:
            raise RuntimeError(f"hftsim failed: {e}") from e
    else:
        proc = subprocess.run([str(binary()), "run", *args], capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError(f"hftsim failed ({proc.returncode}): {proc.stderr.strip()}")
        text = proc.stdout
    result = json.loads(text)
    if cache:
        CACHE.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return result


def run(scenario: Scenario, seed: int, cache: bool = True, use_native: bool = False) -> dict:
    """Run one scenario with one seed."""
    return run_args(scenario.args(seed), cache, use_native)


def run_many(jobs: Iterable[tuple[Scenario, int]], workers: int | None = None, cache: bool = True, progress: bool = True, use_native: bool = False) -> list[dict]:
    """Run (scenario, seed) jobs in parallel (one process each, or one thread each with
    `use_native`); results come back in job order."""
    jobs = list(jobs)
    if not use_native:
        ensure_built()
    workers = workers or max(1, (os.cpu_count() or 2) // 2)
    results: list[dict | None] = [None] * len(jobs)
    with cf.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(run, s, seed, cache, use_native): k for k, (s, seed) in enumerate(jobs)}
        for done, fut in enumerate(cf.as_completed(futures), 1):
            results[futures[fut]] = fut.result()
            if progress:
                print(f"\r  {done}/{len(jobs)} runs", end="", file=sys.stderr, flush=True)
    if progress:
        print(file=sys.stderr)
    return results  # type: ignore[return-value]


def verify(result: dict) -> bool:
    """Re-run a result's manifest (uncached) and check the event digest matches."""
    fresh = run_args(result["manifest"]["args"], cache=False)
    return fresh["manifest"]["digest"] == result["manifest"]["digest"]


def market_makers(result: dict) -> list[dict]:
    return [a for a in result["agents"] if a["kind"] == "market_maker"]


def decomposition(agent: dict, horizon_ms: float) -> dict:
    """The agent's PnL decomposition at one horizon."""
    for d in agent["decomposition"]:
        if abs(d["horizon_ms"] - horizon_ms) < 1e-9:
            return d
    raise KeyError(f"no decomposition at {horizon_ms} ms")
