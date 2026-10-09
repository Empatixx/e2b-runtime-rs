FROM rust:1.98 AS build
RUN apt-get update -qq && apt-get install -y -qq protobuf-compiler musl-tools
WORKDIR /src
COPY . .
RUN target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --release --locked -p envd --target "$target" \
    && cp "target/$target/release/envd" /envd

FROM debian:trixie
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
    && apt-get install -y -qq ca-certificates sudo python3 python-is-python3 nodejs npm procps util-linux \
    && rm -rf /var/lib/apt/lists/*
RUN passwd -d root \
    && useradd -ms /bin/bash user \
    && usermod -aG sudo user \
    && passwd -d user \
    && echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >> /etc/sudoers \
    && chmod 777 -R /home/user /usr/local
COPY --from=build /envd /usr/bin/envd
EXPOSE 49983
ENTRYPOINT ["/usr/bin/envd", "-isnotfc", "-verbose"]
