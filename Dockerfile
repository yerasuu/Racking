# syntax=docker/dockerfile:1
# Build: docker build -t racking-server .   (see docs/deploy.md)

FROM rust:1.90.0-slim-bookworm AS builder
# The slim image ships gcc, which the bundled SQLite needs at build time.
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
# The server embeds the release notes (release.rs) at compile time.
COPY tag-changelog ./tag-changelog
RUN cargo build --release --locked -p racking-server \
    && strip target/release/racking-server

# Same Debian release as the builder, so the glibc matches.
FROM debian:bookworm-slim
RUN useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin racking \
    && mkdir /data /config \
    && chown 10001:10001 /data
COPY --from=builder /src/target/release/racking-server /usr/local/bin/racking-server
# No config, token or data is baked in: mount /config/server.toml read-only
# and keep the database on the /data volume.
WORKDIR /data
VOLUME /data
USER 10001:10001
EXPOSE 7878
# GET /help needs no token; bash is in the image, curl is not.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/7878 && printf 'GET /help HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]
ENTRYPOINT ["racking-server"]
CMD ["--config", "/config/server.toml"]
