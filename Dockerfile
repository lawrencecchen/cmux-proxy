FROM rust:1-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && \
    cargo build --release || true
RUN rm -rf src

# Copy real sources and build
COPY src src
COPY ldpreload ldpreload
COPY tests tests
RUN make -C ldpreload
RUN cargo build --release

# Minimal runtime image
FROM debian:bookworm-slim AS runtime
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates python3 curl unzip && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/cmux-proxy /usr/local/bin/cmux-proxy
COPY --from=builder /app/ldpreload/libworkspace_net.so /usr/local/lib/libworkspace_net.so
ENV LD_PRELOAD=/usr/local/lib/libworkspace_net.so
ENV CMUX_LISTEN=0.0.0.0:8080
EXPOSE 8080
USER root
ENTRYPOINT ["/usr/local/bin/cmux-proxy"]

# Test image (default)
FROM builder AS test
WORKDIR /app
COPY . .
RUN make -C ldpreload && install -m 0755 ldpreload/libworkspace_net.so /usr/local/lib/libworkspace_net.so
ENV LD_PRELOAD=/usr/local/lib/libworkspace_net.so
RUN cargo test --workspace --all-features -- --nocapture
