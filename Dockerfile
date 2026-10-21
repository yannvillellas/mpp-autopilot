FROM rust:1-slim-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked --bin mpp-autopilot \
    && mkdir -p /build/data \
    && chown 999:999 /build/data

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/mpp-autopilot /usr/local/bin/mpp-autopilot
COPY --from=builder --chown=999:999 /build/data /data

ENV STATE_PATH=/data/state.json
WORKDIR /data
USER 999:999
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/mpp-autopilot"]
