-- Account-schema derived QA assessments. Original episodes/facts remain append-only.
CREATE TABLE memory_qa_analyses(
 id text PRIMARY KEY,session_id text NOT NULL,episode_id text NOT NULL,
 rule_version text NOT NULL,input jsonb NOT NULL,output jsonb NOT NULL,
 through_seq bigint NOT NULL,persisted boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX memory_qa_chat_cursor ON memory_qa_analyses(session_id,rule_version,through_seq DESC);
CREATE TABLE memory_qa_artifacts(
 id text PRIMARY KEY,analysis_id text REFERENCES memory_qa_analyses(id) ON DELETE CASCADE,
 kind text NOT NULL CHECK(kind IN ('confirmed','suggestion','temporary','legacy')),
 title text NOT NULL,markdown text NOT NULL,project_name text,session_id text NOT NULL,
 chat_title text,status text NOT NULL,source_event_ids jsonb NOT NULL,
 reason text,evidence jsonb NOT NULL DEFAULT '[]',fact_id text,ordinal integer NOT NULL DEFAULT 0,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX memory_qa_artifact_page ON memory_qa_artifacts(kind,id DESC);
CREATE UNIQUE INDEX memory_qa_legacy_fact ON memory_qa_artifacts(fact_id) WHERE kind='legacy';
CREATE UNIQUE INDEX memory_qa_assessment_ordinal ON memory_qa_artifacts(analysis_id,ordinal);
CREATE TABLE memory_qa_progress(session_id text NOT NULL,rule_version text NOT NULL,through_seq bigint NOT NULL,
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),retry_after timestamptz,PRIMARY KEY(session_id,rule_version));
CREATE TABLE memory_qa_summaries(
 id text PRIMARY KEY,session_id text NOT NULL,rule_version text NOT NULL,
 title text NOT NULL,markdown text NOT NULL,project_name text,chat_title text,
 through_seq bigint NOT NULL,source_event_ids jsonb NOT NULL,updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE(session_id,rule_version)
);
CREATE TABLE memory_qa_trial_analyses(
 trial_id text NOT NULL REFERENCES memory_extraction_trials(id),
 analysis_id text NOT NULL REFERENCES memory_qa_analyses(id) ON DELETE CASCADE,
 PRIMARY KEY(trial_id,analysis_id)
);
ALTER TABLE memory_extraction_trials ADD COLUMN confirmed_count bigint NOT NULL DEFAULT 0;
ALTER TABLE memory_extraction_trials ADD COLUMN suggestion_count bigint NOT NULL DEFAULT 0;
ALTER TABLE memory_extraction_trials ADD COLUMN temporary_count bigint NOT NULL DEFAULT 0;
ALTER TABLE memory_extraction_trials ADD COLUMN summary_count bigint NOT NULL DEFAULT 0;
-- Content-free tombstones prevent re-analysis from resurrecting erased sources.
CREATE TABLE memory_qa_redacted_sources(event_id text PRIMARY KEY);
