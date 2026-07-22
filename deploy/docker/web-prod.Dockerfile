# Production web: build the static bundle, serve it with nginx. The API is
# proxied to CONTROL_PLANE_ORIGIN (default http://control-plane:8080) so the
# app stays same-origin; override the env var to fit any topology.
FROM node:22-slim AS build
RUN corepack enable
WORKDIR /src/frontend
# The whole workspace (tsconfig.base.json et al.) — .dockerignore keeps
# node_modules/dist out of the context.
COPY frontend ./
RUN pnpm install --frozen-lockfile && pnpm --filter @nookos/web build

FROM nginx:1.27-alpine
ENV CONTROL_PLANE_ORIGIN=http://control-plane:8080
COPY deploy/docker/nginx.conf.template /etc/nginx/templates/default.conf.template
COPY --from=build /src/frontend/apps/web/dist /usr/share/nginx/html
EXPOSE 80
