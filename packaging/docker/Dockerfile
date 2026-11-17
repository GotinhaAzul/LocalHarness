# syntax=docker/dockerfile:1
#
# Static, non-root Totality image. Build from the repository root:
#
#   docker build -f packaging/docker/Dockerfile -t totality .
#   docker run --rm -it -v "$PWD:/workspace" totality
#
# The final image is distroless/static: no shell, no package manager, and a
# non-root numeric user. The binary is built for musl in the Alpine stage, so
# it does not depend on the distroless userland at all.

FROM rust:1.88-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/tot /usr/local/bin/tot
USER nonroot:nonroot
ENV HOME=/home/nonroot
# Match the documented bind mount so `tot` derives its project from the
# workspace the user mounted, not from the image's home directory.
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/tot"]
