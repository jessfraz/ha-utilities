CREATE INDEX sync_account_started ON sync_jobs(account_id, started_at DESC, id DESC);
