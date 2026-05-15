# syntax=docker/dockerfile:1
FROM rust:1.95-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY tests ./tests

RUN cargo build --release --locked \
    && cp target/release/monty-server /usr/local/bin/monty-server

FROM gcr.io/distroless/cc-debian12 AS runtime

COPY --from=builder /usr/local/bin/monty-server /usr/local/bin/monty-server

ENV PORT=8080
EXPOSE 8080

USER nonroot
ENTRYPOINT ["/usr/local/bin/monty-server"]
