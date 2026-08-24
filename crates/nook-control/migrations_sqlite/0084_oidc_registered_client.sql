-- SQLite twin of 0079_oidc_registered_client.sql (MAIN-651).
CREATE TABLE IF NOT EXISTS oidc_registered_client (
    issuer TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    registered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
);
