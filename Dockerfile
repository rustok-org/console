# syntax=docker/dockerfile:1
# Binary-carrier image for rustok-console. Not meant to be run directly:
# mcp/Dockerfile.wallet copies /usr/local/bin/rustok-console out of it
# (`COPY --from=console`), the same pattern as the core binary image.

FROM rust:1.95-bookworm AS builder
WORKDIR /usr/src/rustok-console
COPY Cargo.toml Cargo.lock* ./
COPY src/ ./src/
RUN cargo build --release --bin rustok-console

FROM debian:bookworm-slim AS runtime
COPY --from=builder \
    /usr/src/rustok-console/target/release/rustok-console \
    /usr/local/bin/rustok-console
RUN chmod +x /usr/local/bin/rustok-console
ENTRYPOINT ["/usr/local/bin/rustok-console"]
