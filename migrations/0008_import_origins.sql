-- A collector namespace survives consolidation independently of visible ownership.
CREATE INDEX imported_evidence_account ON imported_evidence(account_id,kind,id);
CREATE TABLE import_origins (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    binding TEXT CHECK(binding IS NULL OR json_valid(binding)),
    start_on TEXT,
    consolidation TEXT CHECK(consolidation IS NULL OR json_valid(consolidation))
) WITHOUT ROWID;
CREATE INDEX import_origins_account ON import_origins(account_id,id);
CREATE INDEX import_origins_identity ON import_origins(
    json_extract(binding,'$.deployment'), json_extract(binding,'$.provider'),
    json_extract(binding,'$.provider_account')
) WHERE binding IS NOT NULL;
INSERT INTO import_origins(id,account_id,binding,start_on)
    SELECT id,id,binding,import_start_on FROM accounts
    WHERE import_only=1 OR EXISTS(SELECT 1 FROM import_receipts WHERE account_id=accounts.id);

CREATE TABLE import_origin_receipts (
    origin_id TEXT NOT NULL REFERENCES import_origins(id) ON DELETE CASCADE,
    digest TEXT NOT NULL,
    PRIMARY KEY(origin_id,digest)
) WITHOUT ROWID;
INSERT INTO import_origin_receipts SELECT account_id,digest FROM import_receipts;
DROP TABLE import_receipts;
ALTER TABLE import_origin_receipts RENAME TO import_receipts;
