# Stage 1: Build the static pipeline binary
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src

RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/pipeline /pipeline \
 && strip -s /pipeline

# Stage 2: Minimal scratch runtime container
FROM scratch

COPY --from=builder /pipeline /pipeline

ENV DATA_DIR=/data \
    OUTPUT_DIR=/output \
    SCRATCH_DIR=/scratch \
    HOME=/scratch \
    NCPU=8 \
    MEM_GB=24

USER 1000:1000
ENTRYPOINT ["/pipeline"]
