CREATE TABLE scene_cuts (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL,
  media_path TEXT NOT NULL,
  time_seconds DOUBLE PRECISION NOT NULL CHECK (time_seconds > 0),
  score DOUBLE PRECISION NOT NULL CHECK (score > 0 AND score <= 1),
  threshold DOUBLE PRECISION NOT NULL CHECK (threshold > 0 AND threshold <= 1),
  detected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT fk_scene_cut_media FOREIGN KEY (project_id, media_path)
    REFERENCES media_assets(project_id, original_path)
    ON UPDATE CASCADE ON DELETE CASCADE,
  CONSTRAINT uq_scene_cut_time UNIQUE (project_id, media_path, time_seconds)
);

CREATE INDEX idx_scene_cuts_media_time ON scene_cuts(project_id, media_path, time_seconds);
