-- An explicit bounded job never enables the account's automatic extraction.
CREATE TABLE memory_extraction_trials (
    id ulid PRIMARY KEY,
    status text NOT NULL CHECK(status IN ('running','completed','failed','cancelled')),
    selections jsonb NOT NULL,
    event_ids jsonb NOT NULL,
    total_messages integer NOT NULL CHECK(total_messages BETWEEN 1 AND 8),
    processed_messages integer NOT NULL DEFAULT 0,
    processed_chats integer NOT NULL DEFAULT 0,
    facts_created bigint NOT NULL DEFAULT 0,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE UNIQUE INDEX memory_extraction_one_trial ON memory_extraction_trials((true)) WHERE status='running';
