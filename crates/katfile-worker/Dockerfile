# syntax=docker/dockerfile:1.7
# Build context: repository root (see compose.yaml). Base images are pinned by digest.

FROM rust:1.99.0-slim-trixie@sha256:24e632c09342c20abf8312cf4f61430a911c01ed3a5e4c02b87292b1c39c5273 AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p katfile-worker \
    && cp target/release/katfile-worker /katfile-worker \
    && mkdir -p /out/var/lib/katfile-worker

# Distroless runtime: glibc + CA certificates, no shell, no package manager.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
COPY --from=builder /katfile-worker /usr/local/bin/katfile-worker
# Pre-created with uid 1000 so a fresh named volume inherits writable ownership.
COPY --from=builder --chown=1000:1000 /out/var/lib/katfile-worker /var/lib/katfile-worker
# Same UID/GID as the SFTPGo image so staged files can be snapshotted and cleaned.
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/katfile-worker"]
CMD ["serve"]
