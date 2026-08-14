-- Bringing your own key should also let you bring your own endpoint.
--
-- ## Why this column was added
--
-- The previous migration stored only (provider, key). But a person's "own key" is rarely
-- the official one -- far more often it is a corporate gateway, a relay such as one-api /
-- LiteLLM, some cheaper compatible service, or a self-hosted vLLM. The URL of those
-- endpoints is not the one compiled into the built-in definition, and without this column
-- a key that has been entered still cannot reach where its owner wants it to go.
--
-- This and the server-side `CORTEX_LLM_BASE_URL` are two halves of the same thing: the
-- deployment-level override is for whoever runs the service, the user-level override is
-- for the user.
--
-- NULL = use the one from the built-in definition (the official endpoint).

ALTER TABLE llm_keys ADD COLUMN base_url text;

-- A tombstone row carries no endpoint, for the same reason it carries no ciphertext: a
-- record that says "no longer using an own key" should carry no configuration at all,
-- otherwise every read needs one more judgement about "does this row count or not"
ALTER TABLE llm_keys DROP CONSTRAINT llm_keys_shape;
ALTER TABLE llm_keys ADD CONSTRAINT llm_keys_shape CHECK (
    (deleted AND ciphertext IS NULL AND key_tail IS NULL AND base_url IS NULL)
    OR (NOT deleted AND ciphertext IS NOT NULL AND key_tail IS NOT NULL)
);
