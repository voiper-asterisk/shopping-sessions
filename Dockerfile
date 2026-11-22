# Shopping Sessions: Team FAST native static binary
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu && \
    strip /src/target/x86_64-unknown-linux-gnu/release/shopping-sessions

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/shopping-sessions"]
