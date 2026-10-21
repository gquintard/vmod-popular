# vmod_popular — Agent Context

Varnish VMOD written in Rust. Decides whether a string (typically `req.url`) is
among the most frequently seen ones, so VCL can route hot URLs to a different
backend:

```vcl
import popular;

sub vcl_init {
    new pool = popular.new(1000);   # the ~1000 hottest URLs are popular
}

sub vcl_recv {
    if (pool.is_popular(req.url)) {
        set req.backend_hint = hot;
    } else {
        set req.backend_hint = default;
    }
}
```

Answers are approximate by design: false positives and false negatives are
acceptable. The hard requirement is a fast, non-blocking request path.

## VCL API

| Call | Meaning |
|------|---------|
| `popular.new(INT top, [INT size], [INT window])` | `top`: number of popular entries (> 0). `size`: tracked slots, default `10 * top`, must be in `(top, 2^28]`. `window`: requests between two halvings of all counts, default `1000 * top` capped at `2^30`, must be in `[size, 2^30]`. Bad args fail the VCL load. |
| `BOOL .is_popular(STRING)` | Counts one occurrence, then returns `count > threshold`. |
| `INT .count(STRING)` | Current estimate, does not count. Debug/tests. |
| `INT .threshold()` | Count a string must exceed to be popular. Debug/tests. |
| `VOID .maintain()` | Force a maintenance pass. Tests. |

An unset header is passed as `""`, so it counts as the empty string.
`API.md` is generated from doc comments on build; do not edit it by hand.
`README.md` is hand-written (overview, tuning, building); keep it in sync.

## Layout

- `src/lib.rs` — VMOD glue only: `Pool` wraps a `Table`, argument validation,
  xxh3 hashing of the raw bytes (`CStr::to_bytes()`, no UTF-8 requirement),
  seeded with a random per-pool seed against precomputed collisions.
- `src/table.rs` — the algorithm, plain Rust, no Varnish dependency.
- `src/table/tests.rs` — unit, accuracy and concurrency tests.
- `tests/*.vtc` — varnishtest files, run by `cargo test` via `run_vtc_tests!`.
- `.github/workflows/ci.yml` — fmt, clippy, build, test, bench build and
  `API.md` freshness on Varnish 9.0 and 9.1.
- `.github/workflows/docker.yml` — on tag, pushes a multi-arch
  `ghcr.io/<repo>:<tag>` image built from `Dockerfile` (official `varnish`
  image + the vmod). `dependabot.yml` auto-merges patch updates.
- `benches/hit.rs` — `Table::hit()` throughput at 1/10/100/1000 threads. Includes
  `src/table.rs` via `#[path]` (crate is cdylib-only), no Varnish linkage.

## Algorithm (`src/table.rs`)

HeavyKeeper-style top-K sketch, made lock-free.

**Slots.** `DEPTH = 2` rows of `width = size / 2` `AtomicU64` slots, each packing
`fingerprint:32 | count:32`. Keys are never stored. Slot positions derive from
the fingerprint alone, so maintenance can find a key's other slots from any one
of them. `count == 0` means empty.

**Request path (`Table::hit`)**:
1. `request_number = requests.fetch_add(1) + 1` — the only always-shared write.
2. Decay randomness comes from `fastrand` (thread-local wyrand), drawn only when
   a slot of another key may decay.
3. Per row (`update`): fingerprint matches → `fetch_add(1)`; slot empty → CAS
   to `(fp, 1)`; other key → with probability `1.08^(-count)` CAS `count - 1`,
   taking the slot over if it reaches 0. A failed CAS is ignored, never retried.
4. Estimate = max count over rows holding our fingerprint.
5. If `request_number >= next_maintenance` (a plain load, keeping the mutex off the
   request path), call `maintain(false)`: unlike `maintain(true)`, it skips the pass if no
   tick elapsed since the last one (stale `next_maintenance` read).

**Decision.** Popular iff `estimate > threshold`. Threshold is the `(top+1)`-th
largest count (so at most ~`top` keys are strictly above it), floored at 1 so a
single hit is never popular.

**Maintenance (`Table::maintain(force)`)**, under `Mutex<Maintenance>::try_lock()`:
- `try_lock` failing means a pass is already running: return immediately.
- `requests` sits alone on a 128-byte line (`CachePadded`), so its writes
  don't invalidate the read-mostly fields read on every hit.
- Deadlines come from the request counter, `requests_per_tick = window / 8`. A pass first
  moves `next_maintenance` forward, then halves counts once per window elapsed since
  the previous pass (`count >> windows`, 0 when ≥ 32; empty slots are not written), then recomputes the
  threshold with `select_nth_unstable` over a preallocated `key_counts` `Vec`
  (each key counted once, from its highest slot). O(size), no allocation.
- A skipped pass is never lost: `next_maintenance` stays due until a pass runs, and a
  late pass catches up on all missed windows.

Races are tolerated everywhere (e.g. `fetch_add` hitting a slot that just
changed owner bumps another key's count). Do not "fix" them with locks or CAS
retry loops on the request path.

## Why not existing crates

`heavykeeper` (`TopK`, `CuckooTopK`) needs `&mut self` per insert (a global lock),
clones keys into a heap, and has no window aging. No TinyLFU crate exposes a
usable top-K sketch. `heavykeeper` is a dev-dependency only, as an accuracy
oracle in `accuracy_vs_oracles`.

Possible future work, gated on data: Cuckoo HeavyKeeper-lite (lobby + heavy
slots, no kick chain) behind the same `Table` interface. The oracle test shows it
mainly wins at low skew (Zipf 0.8: recall ~0.98 vs ~0.8 for ours). Also watch
cache-line contention on very hot slots and on the shared request counter.

## Build & test

```sh
cargo build                        # target/debug/libvmod_popular.so
cargo build && cargo test          # unit tests + vtc (needs varnishd/varnishtest, 9.1 here);
                                   # vtc loads target/debug/*.so, which `cargo test` alone does not rebuild
cargo test accuracy -- --nocapture # print recall/precision table vs oracles
cargo clippy --all-targets         # pedantic, must stay clean
cargo bench --bench hit            # hit() throughput; -- --threads 1,12 --dist zipf|uniform|hot --secs 2 --reps 3 --top 1000
```

Test profile uses `opt-level = 3` so the 1M-sample accuracy test stays fast.
VTC tests asserting exact counts use a large `size` to avoid collisions; tiny
tables make exact-count assertions flaky.

## Conventions

- `varnish` crate comes from crates.io (0.8).
- No `unsafe`, no allocation, no blocking lock on the request path.
- Keep `table.rs` free of Varnish types so it stays unit-testable.
- Rust 2024 edition.
