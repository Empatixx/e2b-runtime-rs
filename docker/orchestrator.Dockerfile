FROM ubuntu:24.04 AS build
ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.98.0
RUN apt-get update -qq \
    && apt-get install -y -qq curl ca-certificates build-essential cmake clang libclang-dev pkg-config protobuf-compiler musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p orchestrator \
    && cp target/release/orchestrator /orchestrator
RUN target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --release --locked -p envd --target "$target" \
    && cp "target/$target/release/envd" /envd

FROM ubuntu:24.04 AS assets
ARG DEBIAN_FRONTEND=noninteractive
ARG BUCKET=https://storage.googleapis.com/e2b-artifact-binaries
ARG FIRECRACKER_VERSION=v1.14-0.2.0
ARG FIRECRACKER_SHA256=ef22aec7cbffcf6cc44a8436a4db79f9e6fe5c52218c81af321dd20f10ad6e5d
ARG KERNEL_VERSION=vmlinux-6.1.177_5008931
ARG KERNEL_SHA256=9191ced12d24e6e381753a7ab12ec850877524d453eae75c20aeb176f3b5ad05
ARG BUSYBOX_VERSION=1.36.1
ARG BUSYBOX_SHA256=d7cce939adb09a41a22a5f846d22ba8d576b38dbb2b46a5c77a3a3e27ec52520
RUN apt-get update -qq && apt-get install -y -qq curl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN fetch() { mkdir -p "$(dirname "$2")" && curl -fsSL --retry 3 -o "$2" "$BUCKET/$1" && echo "$3  $2" | sha256sum -c - && chmod "$4" "$2"; } \
    && fetch "firecrackers/$FIRECRACKER_VERSION/amd64/firecracker" "/host/fc-versions/$FIRECRACKER_VERSION/amd64/firecracker" "$FIRECRACKER_SHA256" 0755 \
    && fetch "kernels/$KERNEL_VERSION/amd64/vmlinux.bin" "/host/fc-kernels/$KERNEL_VERSION/amd64/vmlinux.bin" "$KERNEL_SHA256" 0644 \
    && fetch "busybox/$BUSYBOX_VERSION/amd64/busybox" "/host/fc-busybox/$BUSYBOX_VERSION/amd64/busybox" "$BUSYBOX_SHA256" 0755

FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq && apt-get install -y -qq util-linux ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=assets /host /opt/e2b/host
COPY --from=build /orchestrator /opt/e2b/host/var/lib/e2b/bin/orchestrator
COPY --from=build /envd /opt/e2b/host/fc-envd/envd
