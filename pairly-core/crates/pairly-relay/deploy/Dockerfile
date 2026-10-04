# Build context: the pairly-core directory, e.g. from there:
#   docker build -f crates/pairly-relay/deploy/Dockerfile -t pairly-relay .
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
# No LTO and two jobs keep the build within ~1.5 GB of RAM, so it fits a small VPS (with swap).
ENV CARGO_PROFILE_RELEASE_LTO=false \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    CARGO_BUILD_JOBS=2
RUN cargo build --release --locked -p pairly-relay \
 && cp target/release/pairly-relay /pairly-relay

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /var/lib/pairly-relay relay \
 && mkdir -p /var/lib/pairly-relay && chown relay /var/lib/pairly-relay
COPY --from=build /pairly-relay /usr/local/bin/pairly-relay
USER relay
VOLUME /var/lib/pairly-relay
EXPOSE 47200/udp
ENTRYPOINT ["/usr/local/bin/pairly-relay"]
