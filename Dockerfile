# The lumenfx-bot server image: one static-ish binary on a slim base.
FROM rust:1.97-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY migrations migrations
COPY examples examples
RUN cargo build --release --locked --bin lumenfx-bot

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/lumenfx-bot /usr/local/bin/lumenfx-bot
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/lumen-fx/bot" \
      org.opencontainers.image.revision="${REVISION}"
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["lumenfx-bot"]
CMD ["serve"]
