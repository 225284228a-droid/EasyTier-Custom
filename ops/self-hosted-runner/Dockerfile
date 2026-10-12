FROM debian:12-slim

ARG RUNNER_VERSION=2.338.0
ARG RUNNER_PACKAGE=actions-runner-linux-x64-2.338.0.tar.gz
ARG RUNNER_SHA256=af4b794c1bc41d73d40535e3fe092a39f9679cd8d965954c2aca25a05ca41d32
ARG BUILD_SOCKS5_PROXY

ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/home/runner \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    RUNNER_TOOL_CACHE=/cache/tools \
    XDG_CONFIG_HOME=/cache/config

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      binutils build-essential ca-certificates clang curl file git jq \
      libicu-dev libkrb5-3 liblttng-ust1 lld mold musl-tools privoxy \
      python3-minimal tar util-linux zlib1g \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 1000 --user-group --create-home runner
COPY ${RUNNER_PACKAGE} /tmp/actions-runner.tar.gz
RUN set -eux; \
    gzip -t /tmp/actions-runner.tar.gz; \
    echo "${RUNNER_SHA256}  /tmp/actions-runner.tar.gz" | sha256sum -c -; \
    mkdir -p /actions-runner; \
    tar -xzf /tmp/actions-runner.tar.gz -C /actions-runner; \
    rm -f /tmp/actions-runner.tar.gz; \
    cd /actions-runner; \
    ./bin/installdependencies.sh; \
    chown -R runner:runner /actions-runner

WORKDIR /actions-runner
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
RUN set -eux; \
    ok=false; \
    proxy_opts=; \
    if [ -n "${BUILD_SOCKS5_PROXY:-}" ]; then proxy_opts="--proxy $BUILD_SOCKS5_PROXY"; fi; \
    for attempt in $(seq 1 10); do \
      if curl --http1.1 --fail --location --continue-at - \
        --connect-timeout 20 --speed-limit 20480 --speed-time 60 --max-time 180 \
        $proxy_opts https://sh.rustup.rs -o /tmp/rustup-init.sh; then \
        ok=true; break; \
      fi; \
      sleep 5; \
    done; \
    test "$ok" = true; \
    if [ -n "${BUILD_SOCKS5_PROXY:-}" ]; then \
      export HTTPS_PROXY="$BUILD_SOCKS5_PROXY" HTTP_PROXY="$BUILD_SOCKS5_PROXY" \
        https_proxy="$BUILD_SOCKS5_PROXY" http_proxy="$BUILD_SOCKS5_PROXY"; \
    fi; \
    sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain none; \
    rm -f /tmp/rustup-init.sh; \
    ln -s /usr/local/cargo/bin/* /usr/local/bin/; \
    mkdir -p /cache/tools /cache/cargo /cache/rustup /cache/config; \
    chown -R runner:runner /cache

ENV RUSTUP_HOME=/cache/rustup CARGO_HOME=/cache/cargo
COPY runner-entrypoint.sh /usr/local/bin/runner-entrypoint
COPY runner-job-started.sh /usr/local/bin/easytier-job-started
COPY runner-job-completed.sh /usr/local/bin/easytier-job-completed
COPY runner-proxy.conf /etc/privoxy/easytier.conf
RUN chmod 0555 /usr/local/bin/runner-entrypoint /usr/local/bin/easytier-job-started /usr/local/bin/easytier-job-completed \
 && chmod 0444 /etc/privoxy/easytier.conf \
 && printf 'confdir /etc/privoxy\nlogdir /tmp\nlogfile privoxy.log\nlisten-address 127.0.0.1:8118\ntoggle 1\nenable-remote-toggle 0\nenable-remote-http-toggle 0\nenable-edit-actions 0\nenforce-blocks 0\nactionsfile default.action\nfilterfile default.filter\nactionsfile user.action\nfilterfile user.filter\n' > /etc/privoxy/config \
 && cat /etc/privoxy/easytier.conf >> /etc/privoxy/config \
 && chmod 0444 /etc/privoxy/config

USER runner
ENV HTTPS_PROXY=http://127.0.0.1:8118 \
    HTTP_PROXY=http://127.0.0.1:8118 \
    NO_PROXY=localhost,127.0.0.1 \
    https_proxy=http://127.0.0.1:8118 \
    http_proxy=http://127.0.0.1:8118 \
    no_proxy=localhost,127.0.0.1 \
    EASYTIER_CACHE=/cache \
    EASYTIER_CACHE_ACTIVE_MARKER=/cache/.job-active \
    ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/bin/easytier-job-started \
    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/bin/easytier-job-completed
ENTRYPOINT ["/usr/local/bin/runner-entrypoint"]
