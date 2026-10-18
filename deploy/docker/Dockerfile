# syntax=docker/dockerfile:1
# Build from the repository root:  docker build -f deploy/docker/Dockerfile -t orbit-cloud .

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Orbit is a git dependency, so cargo needs git and network access here.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p orbitd -p occtl \
    && install -D target/release/orbitd /out/orbitd \
    && install -D target/release/occtl /out/occtl \
    && install -d -m 0750 /out/data

# distroless/cc: glibc and libgcc only, no shell, no package manager; the default user is `nonroot` (65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/orbitd /out/occtl /usr/local/bin/
# The data directory is created owned by the runtime user, so a fresh named volume mounted on it is writable.
COPY --from=build --chown=65532:65532 /out/data /var/lib/orbit-cloud
ENV ORBIT_CLOUD_DATA=/var/lib/orbit-cloud \
    ORBIT_CLOUD_LISTEN=0.0.0.0:8080
VOLUME ["/var/lib/orbit-cloud"]
EXPOSE 8080
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/orbitd", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/orbitd"]
