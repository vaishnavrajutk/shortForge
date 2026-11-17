CREATE TABLE auto_highlights (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL,
  media_path TEXT NOT NULL,
  start_seconds DOUBLE PRECISION NOT NULL CHECK (start_seconds >= 0),
  end_seconds DOUBLE PRECISION NOT NULL CHECK (end_seconds > start_seconds),
  title TEXT NOT NULL,
  reason TEXT NOT NULL,
  score DOUBLE PRECISION NOT NULL CHECK (score >= 0 AND score <= 1),
  detected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT fk_auto_highlight_media FOREIGN KEY (project_id, media_path)
    REFERENCES media_assets(project_id, original_path)
    ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE INDEX idx_auto_highlights_media ON auto_highlights(project_id, media_path, score DESC);
