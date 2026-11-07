-- A file that turns out to hold bytes the archive already has follows the
-- existing generation instead of starting a copy of it.
--
-- Why this exists: a file's identity is (device, agent, logical path). That is
-- the right key for *where* bytes were seen, and the wrong key for *what* they
-- are. Three ordinary events give the same conversation a second identity:
--
--   * reinstalling the CLI without the old device key (a new device id)
--   * the host moving the file (Codex `sessions/` -> `archived_sessions/`,
--     a Claude Code project directory being renamed, a `.orphaned-…` set-aside)
--   * the same session existing on two machines
--
-- Without this table each of them uploads the whole history again and the
-- projector, which derives event ids from the generation, shows the
-- conversation twice. With it, the newly seen file adopts the generation whose
-- prefix the client has proved its local bytes match (`POST
-- /capture/generations/{id}/verify`), and every content stream is stored and
-- projected once.
--
-- Append-only: a later adoption or a later own generation supersedes an
-- earlier one; nothing is updated. ON DELETE CASCADE because purge deletes
-- files and generations explicitly, and a stale adoption row must not be what
-- blocks a purge.

CREATE TABLE capture_generation_adoptions (
    id               ulid        PRIMARY KEY,
    file_id          ulid        NOT NULL REFERENCES capture_files(id) ON DELETE CASCADE,
    generation_id    ulid        NOT NULL REFERENCES capture_generations(id) ON DELETE CASCADE,
    verified_through BIGINT      NOT NULL CHECK (verified_through >= 0),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX idx_capture_generation_adoptions_file
    ON capture_generation_adoptions (file_id, created_at DESC);
CREATE INDEX idx_capture_generation_adoptions_generation
    ON capture_generation_adoptions (generation_id);

COMMENT ON TABLE capture_generation_adoptions IS
    '文件跟随一个已有的代（内容已被客户端证明一致），而不是再上传一份副本。append-only，不进 sync_log。';

-- The generation each file currently follows: whichever is newest among its own
-- generations and its adoptions. Defined once here so every reader (register,
-- projector, brief, export) agrees on it; a reader that kept its own
-- "ORDER BY ordinal DESC" would silently ignore adoptions.
CREATE VIEW capture_current_generations AS
SELECT DISTINCT ON (x.file_id) x.file_id, x.generation_id
  FROM (
        SELECT g.file_id, g.id AS generation_id, g.created_at, 1 AS own
          FROM capture_generations g
        UNION ALL
        SELECT a.file_id, a.generation_id, a.created_at, 0 AS own
          FROM capture_generation_adoptions a
       ) x
 ORDER BY x.file_id, x.created_at DESC, x.own DESC, x.generation_id DESC;
