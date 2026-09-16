# syntax=docker/dockerfile:1
# Shared multi-stage build for any TypeScript app in the monorepo.
# Build context MUST be the repo root. Driven by build args from docker-info.json:
#   APP_PATH  e.g. apps/dlob-server                  (location of the app)
#   APP_SCOPE e.g. @velocity-exchange/dlob-server    (turbo --filter target)
#   APP_OUT   e.g. dist | lib                         (the app's build output dir)
#   APP_START e.g. lib/index.js                       (entrypoint inside APP_OUT's parent)
#
# Full-context build (the de-risked path vs `turbo prune` + bun lockfile, which has
# known correctness bugs). `bun install` resolves the whole workspace once; turbo
# builds only the requested package + its workspace deps. The runner copies just the
# app's emitted output and installs the native deps esbuild marks external.
#
# Layer order is deliberate: apt, then manifests + install, then sources. A
# source-only change re-runs `turbo build` alone; apt and `bun install` stay cached.

ARG APP_PATH
ARG APP_SCOPE
ARG APP_OUT=dist
ARG APP_START=dist/index.js

FROM oven/bun:1.3.13 AS builder
WORKDIR /app
# Disable Turborepo anonymous telemetry for the image build.
ENV TURBO_TELEMETRY_DISABLED=1 \
    DO_NOT_TRACK=1
# node-hid (via @ledgerhq/hw-transport-node-hid) falls back to a source build
# when its prebuilt-binary download fails (flaky on CI); the oven/bun image has
# no toolchain for that. Install what the fallback needs so a failed fetch
# cannot fail the image build.
# Kept above every COPY so a source-only change never re-runs apt.
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
    python3 make g++ pkg-config libusb-1.0-0-dev libudev-dev \
    && rm -rf /var/lib/apt/lists/*
# Manifests only, so `bun install` re-runs on dependency changes rather than on
# every source edit. Each workspace member is listed explicitly: a wildcard
# (`packages/*/package.json`) flattens onto one destination path, and adding a
# member without adding it here fails loudly at --frozen-lockfile.
# bunfig.toml carries the supply-chain install policy (exact pins,
# minimumReleaseAge) — copy it so the image build is governed by it too.
COPY package.json bun.lock bunfig.toml turbo.json ./
COPY apps/dlob-server/package.json      ./apps/dlob-server/
COPY apps/keeper-bots-v2/package.json   ./apps/keeper-bots-v2/
COPY apps/usermap-server/package.json   ./apps/usermap-server/
COPY packages/cli-admin/package.json    ./packages/cli-admin/
COPY packages/jit-proxy/package.json    ./packages/jit-proxy/
COPY packages/sdk/package.json          ./packages/sdk/
COPY packages/vaults-sdk/package.json   ./packages/vaults-sdk/
# Frozen: install exactly what the committed lockfile pins, never re-resolve.
RUN bun install --frozen-lockfile
COPY packages/ ./packages/
COPY apps/ ./apps/
ARG APP_SCOPE
RUN bunx turbo run build --filter="${APP_SCOPE}"

FROM node:24-alpine AS runner
ENV NODE_ENV=production
ARG APP_PATH
ARG APP_OUT
ARG APP_START
WORKDIR /app
# Native deps esbuild leaves external (union across apps; harmless extras).
RUN apk add --no-cache --virtual .build python3 make g++ \
 && npm install --no-save --no-audit --no-fund \
      bigint-buffer@1.1.5 \
      @triton-one/yellowstone-grpc@6.0.0 \
      helius-laserstream@0.8.5 \
      rpc-websockets@7.5.1 \
 && apk del .build
COPY --from=builder /app/${APP_PATH}/${APP_OUT} ./${APP_OUT}
ENV APP_START=${APP_START}
# Run as a non-root user (uid 1001, primary gid 0) so the k8s workloads can set
# `runAsNonRoot: true` for the file-mount secret hardening. gid 0 + `chmod -R g=u`
# follow the OpenShift arbitrary-uid convention: the app keeps full read/write to its
# working dir under any assigned non-root uid, and a secret volume mounted with
# `fsGroup: 0` (mode 0440) stays group-readable.
RUN adduser -D -u 1001 -G root nonroot \
 && chown -R 1001:0 /app \
 && chmod -R g=u /app
USER 1001
CMD ["sh", "-c", "node ${APP_START}"]
