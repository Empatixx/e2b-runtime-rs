FROM rust:1.98
RUN apt-get update -qq \
    && apt-get install -y -qq protobuf-compiler e2fsprogs iproute2 iptables nftables rsync \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt
