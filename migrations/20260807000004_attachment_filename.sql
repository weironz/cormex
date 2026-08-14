-- Original filename of an attachment
--
-- [Why it belongs to the reference (episode_blobs) and not to the content (blobs)]
--
-- blobs is content-addressed: the hash is decided by the bytes, so one piece of content
-- has exactly one row. And **the same content can perfectly well carry several
-- filenames** -- the same picture is `design-draft.png` today and arrives forwarded next
-- week as `IMG_2043.png`, and both uploads land on the same hash. Putting filename into
-- blobs would mean "whoever uploads first gets to name it", and everybody after that sees
-- somebody else's name; letting the later write win instead would break append-only, and
-- would also change the filename on that message in some other session out of nowhere.
--
-- A filename is a property of **this particular reference**, not of the content, so it
-- belongs to episode_blobs. mime and size_bytes are the other way round: they are uniquely
-- determined by the bytes, they already live in blobs, and a JOIN at read time is all it
-- takes -- there is no reason to keep a second copy here (keeping two copies is how they
-- start disagreeing).
--
-- [Why it is nullable]
--
-- Old data has no filename and none can be backfilled (the original filename was never
-- recorded). On NULL a client should fall back to the "Document · <first 8 of the hash>"
-- rendering, exactly as it did before this change.
ALTER TABLE episode_blobs
    ADD COLUMN filename TEXT;

-- The cap is defensive: filenames come from the client, and with no cap a 10 MB
-- "filename" would go into sync_log and be shipped to every device. 255 matches the
-- per-component limit of mainstream filesystems.
ALTER TABLE episode_blobs
    ADD CONSTRAINT episode_blobs_filename_len
    CHECK (filename IS NULL OR (length(filename) BETWEEN 1 AND 255));

COMMENT ON COLUMN episode_blobs.filename IS
    'The original filename as of this reference. Under content addressing one blob can
     carry several filenames, so this belongs to the reference rather than the content.
     NULL = unknown (old data, or the client supplied none).';
