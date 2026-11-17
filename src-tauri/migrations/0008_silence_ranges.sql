CREATE TABLE silence_ranges (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL,
  media_path TEXT NOT NULL,
  start_seconds DOUBLE PRECISION NOT NULL CHECK (start_seconds >= 0),
  end_seconds DOUBLE PRECISION NOT NULL CHECK (end_seconds > start_seconds),
  noise_threshold_db DOUBLE PRECISION NOT NULL CHECK (noise_threshold_db >= -60 AND noise_threshold_db <= -10),
  minimum_duration_seconds DOUBLE PRECISION NOT NULL CHECK (minimum_duration_seconds > 0),
  detected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT fk_silence_media FOREIGN KEY (project_id, media_path)
    REFERENCES media_assets(project_id, original_path)
    ON UPDATE CASCADE ON DELETE CASCADE,
  CONSTRAINT uq_silence_range UNIQUE (project_id, media_path, start_seconds, end_seconds)
);

CREATE INDEX idx_silence_ranges_media_start
  ON silence_ranges(project_id, media_path, start_seconds);
