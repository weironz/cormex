-- Derived display statistics. Projection identity invalidates stale counts.
CREATE TABLE archive_message_counts (
    session_id ulid PRIMARY KEY REFERENCES capture_sessions(id) ON DELETE CASCADE,
    projection_id ulid NOT NULL,
    parser_version TEXT NOT NULL,
    rule_version TEXT NOT NULL,
    event_count BIGINT NOT NULL CHECK (event_count >= 0),
    message_count BIGINT CHECK (message_count >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
