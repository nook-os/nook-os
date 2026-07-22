# Dev web container: Vite dev server with hot reload (frontend/ is bind-mounted).
FROM node:22-slim
RUN corepack enable
WORKDIR /app/frontend
COPY frontend/package.json frontend/pnpm-workspace.yaml frontend/pnpm-lock.yaml* ./
EXPOSE 5173
CMD ["sh", "-c", "pnpm install && pnpm --filter @nookos/web dev -- --host 0.0.0.0"]
