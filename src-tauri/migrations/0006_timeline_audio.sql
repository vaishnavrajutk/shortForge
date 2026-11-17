CREATE TABLE audio_assets (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  original_path TEXT NOT NULL,
  file_name TEXT NOT NULL,
  duration_seconds DOUBLE PRECISION NOT NULL CHECK (duration_seconds > 0),
  file_size_bytes BIGINT NOT NULL CHECK (file_size_bytes > 0),
  content_sha256 TEXT NOT NULL,
  imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT uq_audio_assets_project_path UNIQUE (project_id, original_path),
  CONSTRAINT uq_audio_assets_project_hash UNIQUE (project_id, content_sha256),
  CONSTRAINT uq_audio_assets_project_id UNIQUE (project_id, id)
);

CREATE TABLE timeline_audio_clips (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  audio_asset_id UUID NOT NULL,
  position_index INTEGER NOT NULL,
  track_kind TEXT NOT NULL CHECK (track_kind IN ('voice', 'music', 'effects')),
  timeline_start_seconds DOUBLE PRECISION NOT NULL CHECK (timeline_start_seconds >= 0),
  source_in_seconds DOUBLE PRECISION NOT NULL CHECK (source_in_seconds >= 0),
  duration_seconds DOUBLE PRECISION NOT NULL CHECK (duration_seconds > 0),
  gain DOUBLE PRECISION NOT NULL DEFAULT 1 CHECK (gain >= 0 AND gain <= 2),
  fade_in_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (fade_in_seconds >= 0),
  fade_out_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (fade_out_seconds >= 0),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT fk_timeline_audio_asset FOREIGN KEY (project_id, audio_asset_id)
    REFERENCES audio_assets(project_id, id) ON DELETE CASCADE,
  CONSTRAINT uq_timeline_audio_position UNIQUE (project_id, position_index),
  CONSTRAINT ck_timeline_audio_source_range CHECK (source_in_seconds + duration_seconds <= 900),
  CONSTRAINT ck_timeline_audio_fades CHECK (fade_in_seconds + fade_out_seconds <= duration_seconds)
);
