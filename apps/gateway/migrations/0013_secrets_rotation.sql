-- Where the SECRETS_KEY reseal sweep is (src/rotation.ts): one row.
CREATE TABLE secrets_rotation (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  rotation TEXT,
  cursor TEXT NOT NULL,
  resealed INTEGER NOT NULL,
  unreadable INTEGER NOT NULL,
  completed_at INTEGER
);
