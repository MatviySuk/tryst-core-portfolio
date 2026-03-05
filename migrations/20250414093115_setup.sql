CREATE TABLE chats (
    chat_id TEXT PRIMARY KEY NOT NULL,
    document BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS chat_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id TEXT NOT NULL,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY(chat_id) REFERENCES chats(chat_id)
);

CREATE INDEX idx_chat_log_chat_id ON chat_log(chat_id);
