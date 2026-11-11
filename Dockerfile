# syntax=docker/dockerfile:1
# Runtime image for legado-server. The musl static binary is built in CI
# (one per architecture) and copied in; the image only adds the
# directories the server needs. No build tools or source in the image.
# Local build: build the musl binary first, then
#   mkdir -p bin-amd64 && cp crates/server/target/x86_64-unknown-linux-musl/release/legado-server bin-amd64/legado-server
#   docker build -t legado-server .
ARG TARGETARCH=amd64
FROM alpine:3.21
RUN mkdir -p /tmp /data /books
COPY bin-${TARGETARCH}/legado-server /legado-server
EXPOSE 8080
ENV LEGADO_BOOKS_DIR=/books \
    LEGADO_DATA_DIR=/data \
    LEGADO_ADDR=0.0.0.0:8080
ENTRYPOINT ["/legado-server"]
