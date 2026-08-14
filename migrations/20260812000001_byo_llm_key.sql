-- The user's own LLM API key.
--
-- ## Why it lives in the user's own schema and not in cortex_auth
--
-- This is a secret belonging to **this one user**, and it lives and dies together with his
-- memory: dropping his schema ought to take the key with it. Keeping it in cortex_auth
-- means remembering to clean it up separately, and "forgot to delete the key when deleting
-- the account" raises no error at all.
--
-- ## Why append-only (as everywhere else in this database)
--
-- Changing the key appends a row; deleting the key appends a tombstone row. The reasoning
-- is the same as for fact_events: an in-place UPDATE makes "when was this key changed"
-- permanently unknowable, and that is the only useful piece of information when tracking
-- down "calls started failing from some day onwards".
--
-- Reads take the newest row; `deleted` being true means that row is a tombstone.
--
-- ## Ciphertext format
--
-- `ciphertext` = nonce(12) || AES-256-GCM(plaintext). The key comes from the deployment's
-- `CORTEX_SECRET_KEY` and is **not in the database**. So a database backup that walks out
-- of the door contains no usable key -- which is the entire point of storing it encrypted.
--
-- `key_tail` is the **last 4 characters** of the plaintext key, used only for recognizing
-- "which one is in there" in the UI. Four trailing characters are not enough to recover
-- anything, and without them the user sees a blank settings page and cannot tell whether
-- he ever entered one.

CREATE TABLE llm_keys (
    id          ulid PRIMARY KEY,
    -- Provider id, matching cortex-llm's provider names (deepseek / anthropic / ...)
    provider    text        NOT NULL,
    -- NULL on a tombstone row
    ciphertext  bytea,
    key_tail    text,
    -- true = this row is the tombstone for "no longer using an own key"
    deleted     boolean     NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),

    -- A tombstone carries no ciphertext and a normal row must carry one. Get it the wrong
    -- way round and it blows up at insert time, instead of at some later call that walks a
    -- NULL into the decrypter
    CONSTRAINT llm_keys_shape CHECK (
        (deleted AND ciphertext IS NULL AND key_tail IS NULL)
        OR (NOT deleted AND ciphertext IS NOT NULL AND key_tail IS NOT NULL)
    )
);

-- Only "the newest row" is ever queried, so take the first one in descending time order
CREATE INDEX idx_llm_keys_latest ON llm_keys (created_at DESC);
