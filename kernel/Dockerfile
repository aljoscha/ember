FROM ubuntu:24.04

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        bc \
        bison \
        ca-certificates \
        cpio \
        curl \
        flex \
        gcc \
        gcc-aarch64-linux-gnu \
        git \
        libelf-dev \
        libssl-dev \
        make \
        python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
