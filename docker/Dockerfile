# syntax=docker/dockerfile:1.7

FROM rust:1.93.0-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        git \
        libclang-dev \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/fiber
ENV CARGO_TARGET_DIR=/usr/src/fiber/target

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p fiber-bin -p fnn-cli \
    && cargo build --release --locked --manifest-path migrate/Cargo.toml

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        libstdc++6 \
        tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /fiber
ENV FIBER_HOME=/fiber

COPY --from=builder /usr/src/fiber/target/release/fnn /usr/local/bin/fnn
COPY --from=builder /usr/src/fiber/target/release/fnn-cli /usr/local/bin/fnn-cli
COPY --from=builder /usr/src/fiber/target/release/fnn-migrate /usr/local/bin/fnn-migrate
COPY config /usr/local/share/fiber/config
COPY docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 8227 8228
VOLUME ["/fiber"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD []
