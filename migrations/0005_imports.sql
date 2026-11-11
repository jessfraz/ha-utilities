ALTER TABLE accounts ADD COLUMN import_only INTEGER NOT NULL DEFAULT 0 CHECK(import_only IN (0,1));
ALTER TABLE accounts ADD COLUMN import_start_on TEXT;
ALTER TABLE accounts ADD COLUMN imported_at TEXT;
CREATE INDEX accounts_import_identity ON accounts(
    deployment, json_extract(binding, '$.provider'), json_extract(binding, '$.provider_account')
) WHERE import_only=1;

CREATE TABLE import_documents (
    deployment TEXT NOT NULL,
    digest TEXT NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY(deployment,digest)
) WITHOUT ROWID;

CREATE TABLE imported_records (
    account_id TEXT NOT NULL REFERENCES accounts(id),
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    bill_id TEXT,
    revision TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    PRIMARY KEY(account_id,kind,id)
) WITHOUT ROWID;
CREATE INDEX imported_bill_records ON imported_records(account_id,bill_id,kind,id) WHERE bill_id IS NOT NULL;

CREATE TABLE imported_revisions (
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    revision TEXT NOT NULL,
    facts TEXT NOT NULL CHECK(json_valid(facts)),
    sources TEXT NOT NULL CHECK(json_valid(sources)),
    PRIMARY KEY(account_id,kind,id,revision),
    FOREIGN KEY(account_id,kind,id) REFERENCES imported_records(account_id,kind,id)
) WITHOUT ROWID;

CREATE TABLE imported_evidence (
    deployment TEXT NOT NULL,
    digest TEXT NOT NULL,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    PRIMARY KEY(deployment,digest,account_id,kind,id),
    FOREIGN KEY(deployment,digest) REFERENCES import_documents(deployment,digest),
    FOREIGN KEY(account_id,kind,id) REFERENCES imported_records(account_id,kind,id)
) WITHOUT ROWID;

CREATE TABLE import_receipts (
    account_id TEXT NOT NULL REFERENCES accounts(id),
    digest TEXT NOT NULL,
    PRIMARY KEY(account_id,digest)
) WITHOUT ROWID;
