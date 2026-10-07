FROM debian:trixie
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
    && apt-get install -y -qq systemd-sysv ca-certificates curl sudo python3 python-is-python3 nodejs npm procps util-linux iproute2 \
    && rm -rf /var/lib/apt/lists/*
RUN passwd -d root \
    && useradd -ms /bin/bash user \
    && usermod -aG sudo user \
    && passwd -d user \
    && echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >> /etc/sudoers \
    && chmod 777 -R /home/user /usr/local
COPY envd /usr/bin/envd
COPY envd.service /etc/systemd/system/envd.service
RUN systemctl enable envd.service \
    && systemctl mask serial-getty@ttyS0.service getty@tty1.service systemd-networkd-wait-online.service
