CREATE TABLE IF NOT EXISTS tryst (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT ,
    current_secret BLOB NOT NULL,
    previous_secret BLOB,
    last_updated_epoch INTEGER NOT NULL,
    is_verified BOOLEAN NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_tryst_account_id ON tryst(account_id) WHERE account_id IS NOT NULL;
