# By using the FROM --platform= pattern we can reuse some steps between platforms.
# For now we can't use cross-compilation for the build step as not all our build tools support
# cross-compilation.
# https://docs.docker.com/build/building/multi-platform
# https://www.docker.com/blog/faster-multi-platform-builds-dockerfile-cross-compilation-guide/

### Build trixter

FROM rust:alpine AS trixter_build

RUN apk add --no-cache --no-progress \
    git make bash curl wget zip gnupg coreutils gcc g++  zstd binutils ca-certificates upx \
    lld mold musl musl-dev cmake clang clang-dev openssl openssl-dev zstd
RUN update-ca-certificates

WORKDIR /trixter
COPY . ./
RUN make clean
RUN make build

## Final image
FROM debian:bookworm-slim AS trixter_runtime

ENV USER=trixter
ENV UID=10001

RUN apt-get update && apt-get upgrade -y && \
    apt-get install -y --no-install-recommends ca-certificates wget && \
    rm -rf /var/lib/apt/lists/*
RUN update-ca-certificates

ENV TZ="UTC"
RUN echo "${TZ}" > /etc/timezone

RUN useradd \
    --home-dir "/home/${USER}" \
    --create-home \
    --shell "/usr/sbin/nologin" \
    --uid "${UID}" \
    "${USER}"

# Copy our build
COPY --from=trixter_build /trixter/dist/trixter /usr/local/bin/trixter

# The final working directory
WORKDIR /home/trixter
USER trixter

ENTRYPOINT ["/usr/local/bin/trixter"]

EXPOSE 8080 8443
