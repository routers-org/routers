FROM rust:1.96-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --bin orchestrator -p routers_realtime

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /build/target/release/orchestrator /orchestrator
ENTRYPOINT ["/orchestrator"]
