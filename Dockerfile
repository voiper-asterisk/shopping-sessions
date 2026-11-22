# Stage 1: Build the static pipeline binary
FROM rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src

RUN cargo build --release --target x86_64-unknown-linux-gnu \
 && cp target/x86_64-unknown-linux-gnu/release/pipeline /pipeline

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
