FROM rust:1.88-alpine

RUN apk add --no-cache musl-dev git

WORKDIR /app

# Pre-fetch dependencies to cache the registry layer.
COPY Cargo.toml ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && cargo fetch

EXPOSE 6025

# Build from the volume-mounted source on each startup (incremental via named volume).
CMD ["sh", "-c", "cargo build --release && ./target/release/git-agent"]
