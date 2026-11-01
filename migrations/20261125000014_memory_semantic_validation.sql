-- Preserve the first pass so a validator retry does not repeat extraction.
ALTER TABLE memory_qa_analyses ADD COLUMN proposed_output jsonb;
ALTER TABLE memory_qa_analyses ADD COLUMN auto_verified boolean NOT NULL DEFAULT false;
