FROM rust:1.75-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -g users botuser

WORKDIR /app

COPY --from=builder /app/target/release/gas-auction-bot /usr/local/bin/gas-auction-bot
COPY config.toml ./config.toml

RUN mkdir -p logs && chown -R botuser:users /app

USER botuser

ENV RUST_LOG=info

EXPOSE 9090

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -sf http://127.0.0.1:9090/ready || exit 1

ENTRYPOINT ["gas-auction-bot"]
