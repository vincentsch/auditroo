# auditroo in a container:
#   docker build -t auditroo .
#   docker run --rm auditroo check https://example.com
FROM rust:1.92-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release && strip target/release/auditroo

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 auditroo
COPY --from=build /src/target/release/auditroo /usr/local/bin/auditroo
USER auditroo
WORKDIR /work
ENTRYPOINT ["auditroo"]
CMD ["--help"]
