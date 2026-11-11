ALTER TABLE accounts ADD COLUMN automatic_started_at TEXT;
ALTER TABLE accounts ADD COLUMN initial_history_finished_at TEXT;
ALTER TABLE accounts ADD COLUMN newest_provider_data_at TEXT;
ALTER TABLE sync_jobs ADD COLUMN scheduled_for TEXT;
ALTER TABLE sync_jobs ADD COLUMN initial_history INTEGER NOT NULL DEFAULT 0
    CHECK (initial_history IN (0, 1));

CREATE UNIQUE INDEX sync_one_scheduled_per_day ON sync_jobs(account_id, scheduled_for)
    WHERE scheduled_for IS NOT NULL;
CREATE INDEX sync_initial_attempts ON sync_jobs(account_id, started_at, state)
    WHERE initial_history = 1;
