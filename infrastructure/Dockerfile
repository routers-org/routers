# syntax=docker/dockerfile:1.7
# One builder for every service binary; pick an image with --target.
FROM rust:1.96-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# The registry and target caches persist across builds, so a source change
# recompiles only the crates it touched.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release -p routers_realtime --bin orchestrator --bin matcher --bin materializer \
    && cargo build --release -p routers_rpc --features server --bin timezone \
    && mkdir -p /out \
    && cp target/release/orchestrator target/release/matcher target/release/materializer target/release/timezone /out/

FROM gcr.io/distroless/cc-debian12 AS orchestrator
COPY --from=builder /out/orchestrator /orchestrator
ENTRYPOINT ["/orchestrator"]

FROM gcr.io/distroless/cc-debian12 AS matcher
COPY --from=builder /out/matcher /matcher
ENTRYPOINT ["/matcher"]

FROM gcr.io/distroless/cc-debian12 AS materializer
COPY --from=builder /out/materializer /materializer
ENTRYPOINT ["/materializer"]

FROM gcr.io/distroless/cc-debian12 AS timezone
COPY --from=builder /out/timezone /timezone
ENTRYPOINT ["/timezone"]
