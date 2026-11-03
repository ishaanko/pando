# syntax=docker/dockerfile:1

# ---- build ----
# The rust image ships gcc, which rusqlite's bundled SQLite needs.
FROM rust:1.99.1-bookworm AS build
WORKDIR /src

# Dependency layer: build against stub sources so this layer is only
# invalidated when Cargo.toml or Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# Make sure cargo sees the real sources as newer than the stubs.
RUN touch src/*.rs && cargo build --release --locked

# ---- runtime ----
# Same Debian release as the build image, so the glibc matches.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --no-create-home pando \
 && mkdir /data && chown pando:pando /data

COPY --from=build /src/target/release/pando /usr/local/bin/pando

USER pando
# Created and owned by pando above, so a fresh named volume is writable.
# For a bind mount, chown the host directory to uid 10001.
VOLUME /data
EXPOSE 8080 2222

# bash is already in the image, so no curl needed. Assumes the default HTTP
# port; drop or override it if you change --listen.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 && printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' >&3 && read -r status <&3 && [[ $status == *' 200 '* ]]"]

ENTRYPOINT ["pando"]
CMD ["serve", "--data", "/data", "--listen", "0.0.0.0:8080", "--ssh-listen", "0.0.0.0:2222"]
