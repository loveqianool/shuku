# syntax=docker/dockerfile:1
# Runtime image for shuku. The musl static binary is built in CI
# (one per architecture) and copied in; the image only adds the
# directories the server needs. No build tools or source in the image.
# Local build: build the musl binary first, then
#   mkdir -p bin-amd64 && cp crates/server/target/x86_64-unknown-linux-musl/release/shuku bin-amd64/shuku
#   docker build -t shuku .
# TARGETARCH must be declared after FROM: an ARG declared before FROM is
# out of scope inside the stage, and declaring it globally shadows
# BuildKit's predefined per-platform value (it resolves to empty).
FROM alpine:3.21
ARG TARGETARCH
RUN mkdir -p /tmp /data /books
COPY --chmod=755 bin-${TARGETARCH}/shuku /shuku
EXPOSE 8080
ENV SHUKU_HOME=/ \
    SHUKU_ADDR=0.0.0.0:8080
ENTRYPOINT ["/shuku"]
