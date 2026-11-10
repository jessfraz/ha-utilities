CREATE TABLE utility_connections (
    id TEXT PRIMARY KEY NOT NULL,
    deployment TEXT NOT NULL,
    request_id TEXT NOT NULL,
    external_id TEXT NOT NULL UNIQUE,
    utility_id TEXT NOT NULL,
    utility_name TEXT NOT NULL,
    name TEXT NOT NULL,
    currency TEXT NOT NULL,
    progress TEXT NOT NULL CHECK(json_valid(progress)),
    revision INTEGER NOT NULL DEFAULT 0,
    provider_settings_accepted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    checked_at TEXT,
    UNIQUE(deployment, request_id)
);
CREATE INDEX utility_connections_deployment ON utility_connections(deployment, created_at, id);
CREATE INDEX utility_connections_queued ON utility_connections(deployment, created_at, id)
    WHERE json_extract(progress, '$.state') = 'queued';

ALTER TABLE accounts ADD COLUMN deployment TEXT;
ALTER TABLE accounts ADD COLUMN binding TEXT CHECK(binding IS NULL OR json_valid(binding));
ALTER TABLE accounts ADD COLUMN connection_id TEXT REFERENCES utility_connections(id);
CREATE INDEX accounts_deployment ON accounts(deployment, id);
CREATE UNIQUE INDEX accounts_remote_binding ON accounts(
    deployment,
    json_extract(binding, '$.provider'),
    json_extract(binding, '$.customer_id'),
    json_extract(binding, '$.provider_account')
) WHERE binding IS NOT NULL;
