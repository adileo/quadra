# The website (homepage, docs, sandbox) built from the sources, for anyone without the CI. From
# the root of this repository, with the studio (private) as a second build context for the demo
# under /app:
#
#   docker build -f site/Dockerfile --build-context studio=../studio -t quadra-site .
#
# Built on the build machine's own architecture (the output is files): the final image only
# copies them into Caddy. The CI builds on the runner instead and uses Dockerfile.release.

# The engine in WebAssembly, for the sandbox
FROM --platform=$BUILDPLATFORM rust:1.97-trixie AS wasm
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends binaryen && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown \
    && cargo install wasm-bindgen-cli --version 0.2.129 --locked
WORKDIR /quadra
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/quadra/target,sharing=locked \
    sh site/scripts/build-wasm.sh

# The site: the sandbox's projects, the studio under /app, VitePress
FROM --platform=$BUILDPLATFORM node:22-trixie-slim AS site
COPY --from=wasm /quadra /quadra
COPY --from=studio . /studio
ENV STUDIO_SRC=/studio
WORKDIR /quadra/site
RUN --mount=type=cache,target=/root/.npm,sharing=locked \
    npm ci && npm run build

FROM caddy:2.11.9
COPY site/Caddyfile /etc/caddy/Caddyfile
COPY --from=site /quadra/site/.vitepress/dist /srv
