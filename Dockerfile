# ---- builder ----
FROM rust:1-alpine AS builder
# musl-dev: C toolchain for rustls' `ring`; alsa-lib-dev + pkgconf: for the `alsa` crate.
RUN apk add --no-cache musl-dev alsa-lib-dev pkgconf
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
# musl targets link fully statically by default; alsa-lib has to be linked dynamically
# (it loads its config and plugins at runtime).
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release --locked

# ---- runtime: the binary plus alsa-lib ----
FROM alpine:3
RUN apk add --no-cache alsa-lib libgcc
COPY --from=builder /app/target/release/cdplayer /usr/local/bin/cdplayer
ENV PORT=42781 DATA_DIR=/data
VOLUME /data
EXPOSE 42781
HEALTHCHECK --interval=5m --timeout=10s --start-period=1m --retries=3 \
    CMD ["cdplayer", "healthcheck"]
ENTRYPOINT ["cdplayer"]
