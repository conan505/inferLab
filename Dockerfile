# syntax=docker/dockerfile:1.7

FROM rust:1.88-bookworm AS builder

WORKDIR /workspace
COPY . .

RUN cargo build --locked --release \
    --package batch-queue \
    --package control-plane \
    --package cpu-worker \
    --package gateway \
    --package trust-distributor \
    --package trust-renewer

FROM debian:bookworm-slim AS runtime

ARG INFERLAB_VERSION=0.31.0

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 inferlab \
    && useradd --uid 10001 --gid inferlab --no-create-home --shell /usr/sbin/nologin inferlab

WORKDIR /opt/inferlab

COPY --from=builder /workspace/target/release/control-plane /usr/local/bin/control-plane
COPY --from=builder /workspace/target/release/raft-link-proxy /usr/local/bin/raft-link-proxy
COPY --from=builder /workspace/target/release/batch-queue /usr/local/bin/batch-queue
COPY --from=builder /workspace/target/release/cpu-worker /usr/local/bin/cpu-worker
COPY --from=builder /workspace/target/release/gateway /usr/local/bin/gateway
COPY --from=builder /workspace/target/release/trust-distributor /usr/local/bin/trust-distributor
COPY --from=builder /workspace/target/release/trust-renewer /usr/local/bin/trust-renewer
COPY --chown=inferlab:inferlab models/tiny-inferlab-v2.bin /opt/inferlab/models/tiny-inferlab-v2.bin
COPY --chmod=0555 deploy/interview/configure-cluster.sh /usr/local/bin/configure-inferlab-cluster

RUN mkdir --parents /var/lib/inferlab /opt/inferlab/models \
    && chown --recursive inferlab:inferlab /var/lib/inferlab /opt/inferlab

LABEL org.opencontainers.image.title="InferLab" \
      org.opencontainers.image.description="Interview demonstration image for the InferLab distributed inference laboratory" \
      org.opencontainers.image.version="${INFERLAB_VERSION}"

ENV RUST_LOG=info

USER inferlab:inferlab

EXPOSE 7000 8080 8081 8090 8091 9091 9101

CMD ["/usr/local/bin/gateway"]
