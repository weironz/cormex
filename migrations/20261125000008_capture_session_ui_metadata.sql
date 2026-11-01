-- Host-provided presentation metadata is separate from the archived transcript.
-- A session can be present on more than one device; each device owns its row.
CREATE TABLE capture_session_ui_metadata (
    session_id ulid NOT NULL REFERENCES capture_sessions(id) ON DELETE CASCADE,
    device_id ulid NOT NULL,
    title TEXT CHECK (title IS NULL OR length(title) <= 4096),
    project_id TEXT CHECK (project_id IS NULL OR length(project_id) <= 512),
    project_name TEXT CHECK (project_name IS NULL OR length(project_name) <= 512),
    pinned_position INTEGER CHECK (pinned_position IS NULL OR pinned_position >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (session_id, device_id)
);
