-- Where the SECRETS_KEY reseal sweep is (src/rotation.ts): one row. scripts/rotate-secrets-key.mjs
-- reads it before it lets a rotation finish.
CREATE TABLE secrets_rotation (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  rotation TEXT,
  slot TEXT,
  cursor TEXT NOT NULL,
  resealed INTEGER NOT NULL,
  unreadable INTEGER NOT NULL,
  completed_at INTEGER,
  checked_at INTEGER
);
