-- Mutable classification metadata for an archived file.
--
-- The transcript and file identity remain append-only. A file's cwd and project
-- are discovery metadata: an old client may have registered them as NULL, or a
-- hook can later provide a more accurate cwd than path decoding. Keeping the
-- correction in a device-owned side table lets existing files be repaired
-- without rewriting capture_files.

CREATE TABLE capture_file_classifications (
    file_id     ulid PRIMARY KEY REFERENCES capture_files(id) ON DELETE CASCADE,
    device_id   ulid NOT NULL,
    cwd         TEXT CHECK (cwd IS NULL OR length(cwd) <= 4096),
    project_key TEXT CHECK (project_key IS NULL OR length(project_key) <= 512),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE capture_file_classifications IS
    '设备可纠正的文件工作目录与项目归属；transcript 与 capture_files 主行仍保持 append-only。';

-- Existing path-derived cwd values are better than "uncategorized". Use a
-- local, machine-specific key until a client supplies a canonical git key.
WITH slashed AS (
    SELECT
        id,
        device_id,
        cwd,
        replace(btrim(cwd), chr(92), '/') AS path
    FROM capture_files
    WHERE project_key IS NULL
      AND cwd IS NOT NULL
      AND btrim(cwd) <> ''
),
unprefixed AS (
    SELECT
        id,
        device_id,
        cwd,
        CASE
            WHEN left(path, 8) ILIKE '//?/UNC/' THEN '//' || substr(path, 9)
            WHEN left(path, 4) ILIKE '//?/' THEN substr(path, 5)
            WHEN left(path, 3) ILIKE '/?/' THEN substr(path, 4)
            ELSE path
        END AS path
    FROM slashed
),
normalized AS (
    SELECT
        id,
        device_id,
        cwd,
        CASE
            WHEN path LIKE '//%' THEN '//' || regexp_replace(substr(path, 3), '/+', '/', 'g')
            ELSE regexp_replace(path, '/+', '/', 'g')
        END AS path
    FROM unprefixed
),
trimmed AS (
    SELECT
        id,
        device_id,
        cwd,
        CASE WHEN length(path) > 1 THEN rtrim(path, '/') ELSE path END AS path
    FROM normalized
),
keys AS (
    SELECT
        id,
        device_id,
        cwd,
        'local:' || CASE
            WHEN path ~ '^[A-Z]:/' THEN lower(left(path, 1)) || substr(path, 2)
            ELSE path
        END AS project_key
    FROM trimmed
)
INSERT INTO capture_file_classifications (file_id, device_id, cwd, project_key)
SELECT id, device_id, cwd, project_key
FROM keys
WHERE length(project_key) <= 512
ON CONFLICT (file_id) DO NOTHING;

-- One effective row for every read path. The presence of a classification row
-- is significant: an explicit NULL project_key overrides the original value,
-- so a client can correct a stale classification instead of only filling it.
CREATE VIEW capture_file_project_keys AS
SELECT
    f.id,
    f.session_id,
    f.device_id,
    f.agent,
    f.logical_path,
    f.role,
    f.framing,
    CASE WHEN c.file_id IS NULL THEN f.cwd ELSE c.cwd END AS cwd,
    CASE WHEN c.file_id IS NULL THEN f.project_key ELSE c.project_key END AS project_key,
    f.first_seen_at
FROM capture_files f
LEFT JOIN capture_file_classifications c ON c.file_id = f.id;
