FROM ghcr.io/cross-rs/armv7-unknown-linux-gnueabihf:main

RUN dpkg --add-architecture armhf && \
    apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    libsoapysdr-dev:armhf \
    && rm -rf /var/lib/apt/lists/*
