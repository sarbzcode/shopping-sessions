# Shopping Sessions: one static native binary on an empty image.
# Scorers are linux/amd64 Intel Xeon (Cascade Lake or newer); x86-64-v2 keeps the binary runnable on
# any x86-64 machine from the last decade, including the dev laptop it is tested on.
FROM rust:1.93-slim-bookworm AS build
WORKDIR /src
ENV RUSTFLAGS="-C target-cpu=x86-64-v2 -C target-feature=+crt-static" \
    CARGO_TARGET=x86_64-unknown-linux-gnu
# Dependencies first, so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
 && cargo build --release --locked --target $CARGO_TARGET \
 && rm -rf src
COPY src ./src
RUN touch src/main.rs \
 && cargo build --release --locked --target $CARGO_TARGET \
 && cp target/$CARGO_TARGET/release/shopping-sessions /shopping-sessions

FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
# The scorer runs as uid 1000 with a read-only root; the binary only writes to $OUTPUT_DIR.
USER 1000:1000
ENTRYPOINT ["/shopping-sessions"]
