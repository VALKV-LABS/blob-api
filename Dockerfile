FROM rust:1.92-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependency build layer separately from source changes
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release --bin blob-api 2>/dev/null; rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --bin blob-api

# ── Runtime image ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/blob-api /usr/local/bin/
EXPOSE 5000
CMD ["blob-api"]
