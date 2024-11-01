FROM rust:latest AS certs
FROM rust:latest AS builder

RUN curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rust-secure-code/cargo-auditable/releases/download/v0.6.4/cargo-auditable-installer.sh | sh

WORKDIR /app
COPY . .

ENV RUSTFLAGS='-C target-feature=+crt-static'
ENV CARGO_INCREMENTAL=0

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo auditable build --locked --profile dist --target x86_64-unknown-linux-gnu && \
    cp ./target/x86_64-unknown-linux-gnu/dist/matrix-sed /matrix-sed

FROM scratch

# Import from builder.

WORKDIR /app

# Copy root certs for tls into image
# You can also mount the certs from the host
# --volume /etc/ssl/certs:/etc/ssl/certs:ro
COPY --from=certs /etc/ssl/certs /etc/ssl/certs

# Copy our build
COPY --from=builder /matrix-sed ./app 

CMD ["/app/app"]
