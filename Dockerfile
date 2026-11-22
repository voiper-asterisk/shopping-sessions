FROM rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
RUN cargo build --release --features w512 --target x86_64-unknown-linux-gnu  && cp target/x86_64-unknown-linux-gnu/release/pipeline /pipeline  && strip -s /pipeline

FROM scratch
COPY --from=build /pipeline /pipeline
ENV DATA_DIR=/data     OUTPUT_DIR=/output     SCRATCH_DIR=/scratch     HOME=/scratch     NCPU=8     MEM_GB=24
USER 1000:1000
CMD ["/pipeline"]
