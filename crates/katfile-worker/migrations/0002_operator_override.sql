-- Set by an operator retry: archive even though the account was disabled or deleted
-- after the upload (into that account's own folder). Never set automatically.
ALTER TABLE upload_jobs ADD COLUMN operator_override INTEGER NOT NULL DEFAULT 0;
