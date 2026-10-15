# syntax=docker/dockerfile:1
#
# Overlay build: recompile the gst-wayland-display compositor from THIS local
# checkout (branch feat/hdr-hevc-main10 -- opt-in 10-bit RGB10A2 / HEVC Main10)
# and layer the resulting plugin + C library onto ghcr.io/games-on-whales/wolf:stable.
#
# Mirrors games-on-whales/wolf docker/wolf.Dockerfile (base image, RUST_VERSION,
# cargo-c 0.10.20, and the cinstall prefix/libdir) but uses `COPY . /src` instead
# of `git clone` so our committed change is what gets built.
#
# Built for linux/amd64 (nv-01 is amd64); on Apple Silicon this runs under QEMU.
ARG GST_BASE=ghcr.io/games-on-whales/gstreamer:1.26.7
ARG WOLF_BASE=ghcr.io/games-on-whales/wolf:stable

FROM ${GST_BASE} AS builder
ENV DEBIAN_FRONTEND=noninteractive
ENV HOME=/root
ENV PATH=/root/.cargo/bin:${PATH}
# Build under x86_64 emulation on a 4-core host: cap parallelism to avoid OOM.
ENV CARGO_BUILD_JOBS=2
RUN apt-get update -y \
 && apt-get install -y --no-install-recommends \
      curl ca-certificates git pkg-config build-essential libssl-dev libicu76 \
      clang libclang-dev \
      libwayland-dev libwayland-server0 libinput-dev libxkbcommon-dev libgbm-dev \
      libudev-dev libdrm-dev libegl-dev libgles-dev libopengl-dev libglib2.0-dev \
      librust-gstreamer-allocators-dev \
 && rm -rf /var/lib/apt/lists/*
ARG RUST_VERSION=1.91.1
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain ${RUST_VERSION} \
 && cargo install cargo-c@0.10.20 --locked
COPY . /src
WORKDIR /src
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo cinstall --features="cuda" \
      --prefix=/usr/local/lib/x86_64-linux-gnu/ \
      --libdir=/usr/local/lib/x86_64-linux-gnu/gstreamer-1.0

FROM ${WOLF_BASE} AS runner
ENV GST_PLUGIN_PATH=/usr/local/lib/x86_64-linux-gnu/gstreamer-1.0/
# Replace wolf:stable's compositor plugin + C lib with our freshly built ones.
COPY --from=builder /usr/local/lib/x86_64-linux-gnu/gstreamer-1.0/* ${GST_PLUGIN_PATH}
COPY --from=builder /usr/local/lib/liblibgstwaylanddisplay* /usr/local/lib/
