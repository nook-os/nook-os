# Production web: build the static bundle, serve it with nginx. The API is
# proxied to CONTROL_PLANE_ORIGIN (default http://control-plane:8080) and team
# chat to CHAT_ORIGIN (default http://chat:8082) so the app stays same-origin;
# override the env vars to fit any topology. Every ${VAR} the template
# references must be defined here — the nginx entrypoint only substitutes vars
# present in the environment, so an undefined one is left as a literal that
# breaks config load. DNS_RESOLVER is where nginx sends the per-request lookups
# for the chat upstream (Docker's embedded DNS by default).
FROM node:22-slim AS build
RUN corepack enable
WORKDIR /src/frontend
# The whole workspace (tsconfig.base.json et al.) — .dockerignore keeps
# node_modules/dist out of the context.
COPY frontend ./
RUN pnpm install --frozen-lockfile && pnpm --filter @nookos/web build

FROM nginx:1.27-alpine
ENV CONTROL_PLANE_ORIGIN=http://control-plane:8080
ENV CHAT_ORIGIN=http://chat:8082
ENV DNS_RESOLVER=127.0.0.11
COPY deploy/docker/nginx.conf.template /etc/nginx/templates/default.conf.template
COPY --from=build /src/frontend/apps/web/dist /usr/share/nginx/html
EXPOSE 80
