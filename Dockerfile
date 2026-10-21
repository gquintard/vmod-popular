# Builds the vmod from this source tree and installs it in the official
# varnish image. Builder and runtime both use Debian trixie.
ARG VARNISH_VERSION=9.1

FROM rust:1-trixie AS builder
ARG VARNISH_VERSION
RUN set -e; \
    curl -Ls https://packages.varnish-software.com/varnish/bootstrap-deb.sh | sh; \
    apt-get install -y libclang-dev "varnish-dev=${VARNISH_VERSION}*"; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /vmod_popular
COPY . .
RUN cargo build --release

FROM varnish:${VARNISH_VERSION}
COPY --from=builder /vmod_popular/target/release/libvmod_popular.so /usr/lib/varnish/vmods/
