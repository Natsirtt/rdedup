FROM rust:1.97.1-bookworm AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY lib lib
COPY sgdata sgdata
COPY tester tester
COPY rdedup-protocol rdedup-protocol
COPY rdedup-server rdedup-server
ENV CARGO_PROFILE_RELEASE_DEBUG=0
RUN cargo build --locked --release -p rdedup -p rdedup-server \
    && strip target/release/rdedup target/release/rdedup-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 rdedup \
    && useradd --uid 10001 --gid rdedup --no-create-home rdedup \
    && mkdir /repository \
    && chown rdedup:rdedup /repository
COPY --from=build /source/target/release/rdedup /usr/local/bin/rdedup
COPY --from=build /source/target/release/rdedup-server /usr/local/bin/rdedup-server
USER 10001:10001
ENV RDEDUP_SERVER_REPOSITORY=/repository \
    RDEDUP_SERVER_LISTEN=0.0.0.0:8080
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["rdedup-server"]
