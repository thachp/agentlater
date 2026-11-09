-- What a SECRETS_KEY rotation resealed over all its passes (src/rotation.ts). `resealed` is only the
-- current pass, and a rotation completes on a pass that reseals nothing, so it always ends at 0.
ALTER TABLE secrets_rotation ADD COLUMN resealed_total INTEGER NOT NULL DEFAULT 0;
