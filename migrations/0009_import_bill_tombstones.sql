CREATE TABLE import_bill_tombstones (
    origin_id TEXT NOT NULL REFERENCES import_origins(id) ON DELETE CASCADE,
    bill_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    removed_services TEXT NOT NULL CHECK(json_valid(removed_services)),
    PRIMARY KEY(origin_id,bill_id)
) WITHOUT ROWID;

-- Retain descendant identities too, so unlinking a payment cannot resurrect it.
CREATE TABLE import_bill_removed_records (
    origin_id TEXT NOT NULL,
    bill_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    record_id TEXT NOT NULL,
    PRIMARY KEY(origin_id,record_id),
    FOREIGN KEY(origin_id,bill_id) REFERENCES import_bill_tombstones(origin_id,bill_id)
        ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX import_bill_removed_family ON import_bill_removed_records(origin_id,bill_id,kind,record_id);
