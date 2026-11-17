ALTER TABLE media_assets
  ADD COLUMN content_sha256 TEXT,
  ADD COLUMN category TEXT NOT NULL DEFAULT 'Uncategorized';

CREATE INDEX IF NOT EXISTS idx_media_project_sha256
  ON media_assets(project_id, content_sha256);
