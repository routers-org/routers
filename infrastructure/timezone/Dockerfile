# syntax=docker/dockerfile:1.7
FROM rust:1.96-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin timezone -p routers_rpc --features server \
    && cp target/release/timezone /timezone

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /timezone /timezone
ENTRYPOINT ["/timezone"]
