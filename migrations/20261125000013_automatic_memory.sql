-- Evolve the derived assessment cache; no original facts are rewritten.
ALTER TABLE memory_qa_artifacts DROP CONSTRAINT memory_qa_artifacts_kind_check;
ALTER TABLE memory_qa_artifacts ADD CONSTRAINT memory_qa_artifacts_kind_check CHECK(kind IN ('confirmed','suggestion','temporary','legacy','memory'));
ALTER TABLE memory_extraction_trials ADD COLUMN memory_count bigint NOT NULL DEFAULT 0;
ALTER TABLE memory_qa_artifacts ADD COLUMN memory_key text;
CREATE INDEX memory_qa_latest_summary ON memory_qa_summaries(session_id,updated_at DESC,id DESC);
CREATE INDEX memory_qa_memory_key ON memory_qa_artifacts(project_name,memory_key) WHERE kind='memory';
