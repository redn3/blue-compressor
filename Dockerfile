FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY app/ui ./app/ui
COPY app/src-tauri ./app/src-tauri
RUN cargo build --release -p bluecompress-cli --features cuda,opencl

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/redn3/blue-compressor"
LABEL org.opencontainers.image.description="Blue Compressor: Web UI, watch-folder daemon, and CLI for Nikon N-RAW (.NEV) and R3D NE (.R3D) raw video recompression"
LABEL org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ocl-icd-libopencl1 ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /media /watch/in /watch/out

COPY --from=builder /src/target/release/bluecompress /usr/local/bin/bluecompress

ENV PORT=9847
ENV BROWSE_ROOT=/
ENV WATCH_INPUT=/watch/in
ENV WATCH_OUTPUT=/watch/out
# 1 = also write a 10-bit H.265 proxy to Proxies/ next to every output (libx265 on the CPU,
# or NVENC / Quick Sync when a GPU and its driver are passed through).
ENV PROXIES=0

EXPOSE 9847

ENTRYPOINT ["bluecompress"]
CMD ["serve"]
