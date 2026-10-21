# vmod-popular

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
