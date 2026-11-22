# Shopping Sessions High-Performance Data Pipeline
# Target: linux/amd64 (Cascade Lake / AVX-512)
FROM rust:1.100-alpine AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=cascadelake -C target-feature=+avx512bw"
RUN cargo build --release --locked && strip /src/target/release/shopping-sessions

FROM scratch
COPY --from=build /src/target/release/shopping-sessions /shopping-sessions
CMD ["/shopping-sessions"]
