# syntax=docker/dockerfile:1.7
# All cubeharbor images from one build. Targets: operator, gateway, dashboard, server.
#   docker buildx bake            (see docker-bake.hcl)
#   docker build --target server --build-arg JAVA_VERSION=21 -t cubeharbor-server:dev-java21 .

ARG RUST_VERSION=1
ARG JAVA_VERSION=21

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The dashboard's tests check this example against the built-in defaults.
COPY examples/site.yaml ./examples/site.yaml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && mkdir -p /out \
 && cp target/release/cubeharbor-operator target/release/cubeharbor-gateway target/release/cubeharbor-dashboard \
       target/release/cubeharbor-installer target/release/cubeharbor-runner target/release/cubeharbor-backup target/release/cubeharbor /out/

# `docker build --target test .` runs the unit tests (used by CI).
FROM build AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo test --release --locked --workspace

FROM gcr.io/distroless/cc-debian12:nonroot AS operator
COPY --from=build /out/cubeharbor-operator /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/cubeharbor-operator"]

FROM gcr.io/distroless/cc-debian12:nonroot AS gateway
COPY --from=build /out/cubeharbor-gateway /usr/local/bin/
EXPOSE 25565
ENTRYPOINT ["/usr/local/bin/cubeharbor-gateway"]

FROM gcr.io/distroless/cc-debian12:nonroot AS dashboard
COPY --from=build /out/cubeharbor-dashboard /usr/local/bin/
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/cubeharbor-dashboard"]

FROM gcr.io/distroless/cc-debian12:nonroot AS cubeharbor
COPY --from=build /out/cubeharbor /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/cubeharbor"]

# Server image: Temurin JRE + the Rust installer (init container) and runner (PID 1).
FROM eclipse-temurin:${JAVA_VERSION}-jre AS server
COPY --from=build /out/cubeharbor-installer /out/cubeharbor-runner /out/cubeharbor-backup /usr/local/bin/
RUN mkdir -p /data /cache && chown 1000:1000 /data /cache
ENV CUBEHARBOR_DATA=/data
WORKDIR /data
USER 1000:1000
EXPOSE 25565 25575 24454/udp 19132/udp
CMD ["cubeharbor-runner"]
