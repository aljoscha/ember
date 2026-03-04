FROM ubuntu:24.04

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        bc \
        bison \
        ca-certificates \
        cpio \
        flex \
        gcc \
        git \
        libelf-dev \
        libssl-dev \
        make \
        python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
