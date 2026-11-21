# Multi-stage build for ultra-lightweight static execution
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src

RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/shopping-sessions /sessions-bin

FROM scratch
COPY --from=build /sessions-bin /shopping-sessions
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/shopping-sessions"]
