# Agent Mom server image (Fly). Runs the node control plane; executes no
# user code — sandboxes are am-ws-* machines, deployments are am-dep-* apps.
FROM node:24-bookworm-slim AS deps
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts && npm rebuild esbuild

FROM node:24-bookworm-slim AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --ignore-scripts && npm rebuild esbuild
COPY tsconfig.json vite.config.ts index.html ./
COPY src ./src
COPY web ./web
COPY technically-speaking ./technically-speaking
RUN npx vite build

FROM node:24-bookworm-slim
WORKDIR /app
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl rsync \
  && rm -rf /var/lib/apt/lists/* \
  && curl -fsSL https://fly.io/install.sh | FLYCTL_INSTALL=/usr/local sh
ENV NODE_ENV=production
COPY --from=deps /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY package.json package-lock.json tsconfig.json ./
COPY src ./src
COPY scripts ./scripts
EXPOSE 7392
CMD ["./node_modules/.bin/tsx", "src/server.ts"]
