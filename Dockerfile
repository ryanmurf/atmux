# syntax=docker/dockerfile:1.7

FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends clang cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --locked --release --all-features

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime

ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="atmux" \
      org.opencontainers.image.description="Tmux control-plane coordinator" \
      org.opencontainers.image.source="https://github.com/ryanmurf/atmux" \
      org.opencontainers.image.revision="${VCS_REF}"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        curl \
        netcat-openbsd \
        tmux \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 atmux \
    && useradd --uid 10001 --gid atmux --no-create-home --home-dir /var/lib/atmux/home atmux

COPY --from=builder /src/target/release/atmux /usr/local/bin/atmux

USER 10001:10001
WORKDIR /
ENTRYPOINT ["/usr/local/bin/atmux"]
