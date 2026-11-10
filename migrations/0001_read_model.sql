CREATE TABLE accounts (
    id TEXT PRIMARY KEY NOT NULL,
    binding_hash TEXT NOT NULL
);

CREATE TABLE observations (
    account_id TEXT NOT NULL REFERENCES accounts(id),
    kind TEXT NOT NULL CHECK(kind IN ('bill','balance','payment','service','collection','usage')),
    id TEXT NOT NULL,
    service_id TEXT,
    sort_key TEXT NOT NULL,
    end_key TEXT,
    observed_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    PRIMARY KEY(account_id,kind,id)
);
CREATE INDEX observations_kind_date ON observations(kind,sort_key,account_id,id);
CREATE INDEX observations_account_date ON observations(account_id,kind,sort_key,id);
CREATE INDEX observations_service_date ON observations(service_id,kind,sort_key,account_id,id);
CREATE INDEX usage_overlap ON observations(service_id,end_key,sort_key,account_id,id) WHERE kind='usage';

CREATE TABLE coverage (
    account_id TEXT NOT NULL REFERENCES accounts(id),
    capability TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    PRIMARY KEY(account_id,capability)
);

CREATE TABLE sync_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    state TEXT NOT NULL CHECK(state IN ('running','complete','partial','failed','interrupted')),
    started_at TEXT NOT NULL,
    finished_at TEXT,
    observations INTEGER NOT NULL DEFAULT 0 CHECK(observations >= 0),
    error TEXT
);
CREATE UNIQUE INDEX sync_one_running ON sync_jobs(account_id) WHERE state='running';
CREATE INDEX sync_account_finished ON sync_jobs(account_id,state,finished_at);
