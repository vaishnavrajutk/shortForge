ALTER TABLE media_assets
  ADD CONSTRAINT uq_media_assets_project_path UNIQUE (project_id, original_path);

CREATE TABLE timeline_clips (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  media_path TEXT NOT NULL,
  position_index INTEGER NOT NULL,
  source_in_frame BIGINT NOT NULL CHECK (source_in_frame >= 0),
  source_out_frame BIGINT NOT NULL CHECK (source_out_frame > source_in_frame),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT fk_timeline_media FOREIGN KEY (project_id, media_path)
    REFERENCES media_assets(project_id, original_path)
    ON UPDATE CASCADE ON DELETE CASCADE,
  CONSTRAINT uq_timeline_position UNIQUE (project_id, position_index)
);
