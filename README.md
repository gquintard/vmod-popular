# vmod-popular

[![CI](https://github.com/gquintard/vmod-popular/actions/workflows/ci.yml/badge.svg)](https://github.com/gquintard/vmod-popular/actions/workflows/ci.yml)

A [Varnish](https://varnish-cache.org/) module, written in Rust, telling whether
a string (typically `req.url`) is among the most frequently seen ones. Use it to
route hot content to a dedicated backend, cache it differently, or just tag it.

```vcl
import popular;

sub vcl_init {
    # the ~1000 most requested URLs are "popular"
    new pool = popular.new(1000);
}

sub vcl_recv {
    if (pool.is_popular(req.url)) {
        set req.backend_hint = hot;
    } else {
        set req.backend_hint = default;
    }
}
```

The full VCL API is described in [API.md](API.md).

## How it works

Each pool is a [HeavyKeeper](https://www.usenix.org/conference/atc18/presentation/gong)-style
top-K sketch, made lock-free:

- strings are hashed, never stored: memory is fixed at creation (8 bytes per
  tracked slot) and nothing is allocated on the request path.
- `is_popular()` counts one occurrence and compares the string's estimated
  count against a threshold, using only atomic operations. No lock is taken
  on the request path.
- a periodic maintenance pass, run by whichever request crosses the deadline,
  recomputes the threshold so that roughly `top` strings are above it.
- every `window` requests, all counts are halved, so strings that stop being
  requested eventually stop being popular.

Answers are **approximate by design**: a string may occasionally be reported
popular when it isn't, or the other way around. Accuracy is best when traffic
is skewed (a few strings getting most hits), which is the usual case for URLs.

## Tuning

`popular.new(INT top, [INT size], [INT window])`:

- `top`: how many strings should be considered popular.
- `size`: how many slots are tracked (default `10 * top`). More slots means
  fewer collisions and better accuracy, at 8 bytes each.
- `window`: how many requests between two halvings of all counts (default
  `1000 * top`, capped at 2^30). Shorter windows react faster to changing
  traffic, longer ones give steadier answers.

Pools are independent: create one per use case (URLs, client IPs, hosts...).
An unset header is passed as `""`, and is counted as the empty string.

## Performance

Numbers below come from a laptop (Intel i7-1365U, 12 threads, Rust 1.99,
`top = 1000` and default `size`/`window` unless stated). They are indicative
only: rerun the benchmarks on your own hardware.

### Request path

`cargo bench --bench hit` measures `is_popular()` throughput (hashing
excluded), in millions of calls per second, all threads combined:

| threads | Zipf 1.1 | uniform (1M keys) | single hot key |
| ------: | -------: | ----------------: | -------------: |
|       1 |       35 |                47 |             50 |
|      10 |       16 |                23 |             13 |
|     100 |       18 |                19 |             13 |
|    1000 |       14 |                18 |             13 |

A single thread spends 20-30 ns per call. Throughput doesn't grow with the
thread count: every call writes a shared request counter, and hot keys share
their slots, so cores mostly wait on each other's cache lines. Even so, a pool
handles over 10 million calls per second, well beyond what a single Varnish
server needs.

### Maintenance

`cargo bench --bench maintain` times the call that triggers a maintenance
pass, since that request pays for the pass:

|     top |      size | pass with halving | threshold-only pass | cost per request |
| ------: | --------: | ----------------: | ------------------: | ---------------: |
|     100 |      1000 |             19 µs |               15 µs |           1.2 ns |
|    1000 |     10000 |            161 µs |              116 µs |           1.0 ns |
|   10000 |    100000 |            1.5 ms |              1.0 ms |           0.9 ns |
|  100000 |   1000000 |             11 ms |              8.1 ms |           0.7 ns |

A pass costs 10-20 ns per slot. Averaged over all requests (8 passes per
default window, one of them halving), that's around 1 ns per request. The
request that runs a pass does see the full delay, though: about 10 ms with a
million slots.

### Accuracy

`cargo bench --bench accuracy` replays a [Zipf](https://en.wikipedia.org/wiki/Zipf%27s_law) stream over 1M keys for 4.5
windows, and checks each `is_popular()` answer against the true 1000 most
likely keys. Cells are recall/precision: the share of requests for a true top
key answered `true`, and the share of `true` answers that were for a true top
key. "Top share" is the fraction of traffic going to the true top keys.

| skew | top share | size = 2 × top | 5 × top   | 10 × top (default) | 20 × top  |
| ---: | --------: | :------------: | :-------: | :----------------: | :-------: |
|  0.7 |      0.11 | 0.44/0.90      | 0.65/0.85 | 0.82/0.77          | 0.94/0.90 |
|  0.9 |      0.35 | 0.79/0.97      | 0.93/0.93 | 0.98/0.98          | 0.99/0.98 |
|  1.1 |      0.69 | 0.94/0.99      | 0.98/0.99 | 1.00/0.99          | 1.00/0.99 |
|  1.3 |      0.91 | 0.98/0.99      | 1.00/1.00 | 1.00/1.00          | 1.00/1.00 |

With the default size, answers are close to exact once traffic is moderately
skewed. With flat traffic, the long tail keeps evicting mid-ranked keys before
they build up a count: increase `size` if `threshold()` stays at 1.

## Building

You need Rust, `varnish-dev` (headers and `pkg-config` files) and
`varnishtest` for the tests.

```sh
cargo build --release   # target/release/libvmod_popular.so
cargo test              # unit tests, then vtc tests against target/debug
```

Install the library in Varnish's vmod directory:

```sh
cp target/release/libvmod_popular.so "$(pkg-config --variable=vmoddir varnishapi)/"
```

## Docker

Each tag pushes `ghcr.io/gquintard/vmod-popular:<tag>`, the official
`varnish` image with the vmod installed. To build it locally:

```sh
docker build -t vmod-popular .                                  # Varnish 9.1
docker build -t vmod-popular --build-arg VARNISH_VERSION=9.0 .
```

## Version matching

| vmod-popular | varnish |
| :----------- | :-----: |
| 0.1.0        | 9.0+    |
