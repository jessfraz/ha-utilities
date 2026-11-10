FROM rust:1.99.1-bookworm AS build
RUN apt-get update \
    && apt-get install --no-install-recommends -y cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY web ./web
RUN cargo build --locked --release --bin utilities --bin ha-utilities

FROM debian:bookworm-slim
ARG BUILD_VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/jessfraz/ha-utilities" \
      org.opencontainers.image.licenses="MIT" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="aarch64|amd64"
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates gosu \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 utilities \
    && useradd --uid 10001 --gid utilities --no-create-home --shell /usr/sbin/nologin utilities \
    && install -d -o utilities -g utilities /data
COPY --from=build /src/target/release/utilities /usr/local/bin/utilities
COPY --from=build /src/target/release/ha-utilities /usr/local/bin/ha-utilities
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
ENV UTILITIES_BIND=0.0.0.0:8080 \
    UTILITIES_DATABASE=/data/utilities.sqlite3
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
