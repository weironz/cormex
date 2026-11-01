-- Account-scoped preferences; each tenant has its own schema.
CREATE TABLE memory_extraction_settings (
    id ulid PRIMARY KEY,
    enabled boolean NOT NULL,
    use_server_model boolean NOT NULL,
    provider text NOT NULL,
    model text NOT NULL,
    base_url text,
    ciphertext bytea,
    key_tail text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- Derived worker health, not a replacement for append-only promote_progress.
CREATE TABLE memory_extraction_work (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    running boolean NOT NULL DEFAULT false,
    last_error text,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
