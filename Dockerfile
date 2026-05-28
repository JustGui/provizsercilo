FROM rust:1.95-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY server/ server/

RUN cargo build --release --bin proviz-sercilo

# -----------------------------------------------------------------------

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/proviz-sercilo /usr/local/bin/proviz-sercilo
COPY profiles.toml /etc/proviz-sercilo/profiles.toml

ENV PORT=8090
ENV PROFILES_PATH=/etc/proviz-sercilo/profiles.toml
ENV DATABASE_PATH=/data/proviz.db
ENV SECRETS_DIR=/run/secrets
ENV LOG_LEVEL=INFO
ENV LOG_FORMAT=json

EXPOSE 8090
VOLUME ["/data"]

ENTRYPOINT ["proviz-sercilo"]
