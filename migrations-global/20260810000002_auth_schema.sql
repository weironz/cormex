-- The foundation of multi-user: accounts, session tokens, invites, usage
--
-- Design basis: docs/roadmap.md, "R9 · Authentication and multi-tenancy".
--
-- ══════════════════════════════════════════════════════════
--  ⚠️ This file is in `migrations-global/`, not in `migrations/`
--
--  Multi-tenancy split the migration set in two, and the distinction is **mandatory**:
--
--  - `migrations/`        runs once per tenant (17 tables + views, one memory per person)
--  - `migrations-global/` runs once for the whole deployment (the account tables, unique
--                         across tenants)
--
--  What happens when they are mixed: creating a new tenant pours the entire migration set
--  into that tenant's schema, and on reaching this one runs into
--  `relation "users" already exists` -- because it is `cortex_auth.users`, fully qualified,
--  and has nothing to do with the tenant schema. **This has actually been hit**, and the
--  point of failure is halfway through account creation: the schema half built, the user
--  record not yet written.
--
--  The `_sqlx_migrations` of the global set lands inside `cortex_auth` (the connection uses
--  `search_path=cortex_auth,public`), so its version numbers never interfere with the
--  tenant set's.
--
-- ══════════════════════════════════════════════════════════
--  Why these tables live in a schema of their own
--
--  The isolation mechanism is **one Postgres schema per user** (not one of the 77 queries
--  has to change, and the isolation is structural -- a connection simply cannot see any
--  other schema). The account tables, however, are **cross-user**: they must sit outside
--  every user schema, otherwise each user would have a user table of his own and "who is
--  allowed to log in" would no longer have a single answer.
--
--  Hence: `cortex_auth` holds the accounts, `public` and `u_<ulid>` hold each person's memory.
--
-- ══════════════════════════════════════════════════════════
--  User number 1's schema is simply called public
--
--  The production database already holds existing data. Moving 17 tables + views into
--  `u_<ulid>` with `ALTER TABLE ... SET SCHEMA` is a piece of DDL with **no way back**, and
--  all it buys is looking tidy.
--  So the schema name is stored in the users table: user number 1 gets `public`, and only
--  new users get `u_<ulid>`.
--
--  Corollary: `users.schema_name` is not a derived value and cannot be computed from the id.
--  Do not add that sort of "optimization".
-- ══════════════════════════════════════════════════════════

-- The schema itself is created by `Store::migrate_global` **before** the migrator runs --
-- because `_sqlx_migrations` has to land inside it, and that happens before the first migration.
-- ══════════════════════════════════════════════════════════
--  Accounts
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.users (
    id            ulid        PRIMARY KEY,
    -- The login name. Unique case-insensitively -- see the functional index below.
    --
    -- Not called email: this deployment has no mail channel (no password recovery, no
    -- verification mail), and calling it email would make people assume those features
    -- exist. Add a column once mail is genuinely wired up.
    username      TEXT        NOT NULL,
    -- The PHC string of argon2id (`$argon2id$v=19$m=...`).
    --
    -- **The SHA-256 digest from cortexd::auth must not be reused here.** That argument
    -- ("a slow KDF is what protects a low-entropy secret from offline cracking") is
    -- correct, but it rests on a premise: that the token is high-entropy and random. A
    -- password chosen by a human does not satisfy it, so this one has to be a slow KDF.
    password_hash TEXT        NOT NULL,
    -- Which schema this person's memory lives in. User number 1 is `public`, see the file header.
    schema_name   TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- Disabled rather than deleted: delete a user row and his schema becomes an orphan
    -- nobody claims, with tens of thousands of memories inside it. Once disabled, login is
    -- refused and the data stays exactly where it was.
    disabled_at   TIMESTAMPTZ,

    CONSTRAINT users_username_shape CHECK (
        username ~ '^[a-zA-Z0-9][a-zA-Z0-9._-]{1,62}$'
    ),
    -- The schema name gets spliced into `search_path`, which makes it an **injection
    -- surface**. Pinning it down to "public, or u_ plus a 26-character lowercase ULID" is
    -- far more dependable than quoting and escaping: escaping has to be remembered at every
    -- splice site, whereas this CHECK only has to be written once.
    --
    -- **It has to be lowercase.** An unquoted identifier inside `search_path` gets folded
    -- to lowercase, while `CREATE SCHEMA "u_ABC"` preserves the case -- and when the two
    -- disagree, `search_path` points at a schema that does not exist, which Postgres
    -- **silently skips**: the new tenant has not a single table created, every write lands
    -- in public, and migrate() returns Ok.
    -- Actually hit in testing (see cortex_store::tenant::SchemaName::new)
    CONSTRAINT users_schema_shape CHECK (
        schema_name = 'public'
        OR schema_name ~ '^u_[0-9a-hjkmnp-tv-z]{26}$'
    ),
    CONSTRAINT users_schema_unique UNIQUE (schema_name)
);

-- Case-insensitive uniqueness.
--
-- A functional index rather than citext: citext is an extension, and this project has
-- already moved Chinese tokenization over to the Rust side because "managed PG cannot
-- install zhparser" (see architecture.md). The same reasoning -- one extension dependency
-- fewer, one more place it can run.
CREATE UNIQUE INDEX idx_users_username ON cortex_auth.users (lower(username));

-- ══════════════════════════════════════════════════════════
--  Session tokens
--
--  Only the digest of a refresh token is stored. The plaintext exists solely in the
--  response body at the instant it is issued -- structurally identical to how cortexd::auth
--  treats the pre-shared token, and for the same reason: someone reading the database must
--  not thereby take over the account.
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.auth_tokens (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    -- SHA-256(plaintext). **A fast digest is the right choice here** -- which does not
    -- contradict the slow KDF on the password column: the criterion is one and the same
    -- (only low entropy needs a slow KDF), and a refresh token is a high-entropy random
    -- string we generated ourselves.
    token_sha256  sha256      NOT NULL UNIQUE,
    -- One rotation chain. Every use of refresh swaps in a new token, and the new one
    -- inherits the same family.
    --
    -- **Replay detection rests on this**: presenting an old token that has already been
    -- rotated means it has leaked, and at that point the whole chain is invalidated
    -- together -- invalidating only that one is not enough, since the attacker holds the
    -- successors too.
    family_id     ulid        NOT NULL,
    issued_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at    TIMESTAMPTZ NOT NULL,
    -- The moment it was spent and exchanged for a new token. Non-null = this one must no
    -- longer be accepted.
    rotated_at    TIMESTAMPTZ,
    -- Explicit invalidation (a logout, or replay detection killing off the whole chain).
    revoked_at    TIMESTAMPTZ,
    -- Whom it was issued to. A purely diagnostic field: when one account is logged in on
    -- three machines, "which machine leaked its credential" can be answered nowhere else
    device_label  TEXT
);

CREATE INDEX idx_auth_tokens_user   ON cortex_auth.auth_tokens (user_id);
-- Replay detection invalidates by family in bulk, so this is a hot path rather than an
-- index for operations work
CREATE INDEX idx_auth_tokens_family ON cortex_auth.auth_tokens (family_id);

-- ══════════════════════════════════════════════════════════
--  Invite codes
--
--  Open registration is **off** by default: anyone being able to register means anyone can
--  burn money on the server's DeepSeek key, and production is a 2-core, 3.5 GB machine that
--  is already running 19 containers.
--  An invite code is the minimal implementation of "let in whoever you want to let in".
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.invites (
    id           ulid        PRIMARY KEY,
    -- Again only the digest is stored. A leaked invite code does less damage than a leaked
    -- token, but there is no reason to keep the plaintext
    code_sha256  sha256      NOT NULL UNIQUE,
    created_by   ulid        REFERENCES cortex_auth.users(id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at   TIMESTAMPTZ,
    used_at      TIMESTAMPTZ,
    used_by      ulid        REFERENCES cortex_auth.users(id),

    -- Once it has been used, who used it must be known. The two columns are null together
    -- or non-null together -- letting them diverge would produce an intermediate state in
    -- which "who used this code" cannot be answered at all
    CONSTRAINT invites_used_together CHECK (
        (used_at IS NULL) = (used_by IS NULL)
    )
);

-- ══════════════════════════════════════════════════════════
--  Usage
--
--  One row appended per call, in keeping with the append-only discipline of the whole
--  database: a quota is a number that gets disputed ("I certainly did not use that much"),
--  and only a per-call record can answer that.
--
--  Deliberately **no** "current usage" column -- maintaining one would take an UPDATE, and
--  UPDATE is permitted in this project only inside redact/purge. Usage is computed by
--  aggregating over a time window.
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.usage (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 'chat' / 'extract' / 'embed' / 'transcribe' -- which pipeline spent the money.
    -- No CHECK enumeration: adding a new pipeline should not require a migration
    kind          TEXT        NOT NULL,
    input_tokens  BIGINT      NOT NULL DEFAULT 0,
    output_tokens BIGINT      NOT NULL DEFAULT 0,
    -- Whether it went through the server's key or the user's own one.
    --
    -- **They have to be recorded apart**: usage on an own key does not count against the
    -- quota, but still has to be visible -- "how much did I spend this month" and "how much
    -- quota do I have left" are two different questions
    own_key       BOOLEAN     NOT NULL DEFAULT FALSE,
    model         TEXT
);

-- The shape of a quota query is always "this user, this period, the ones that count against
-- the quota"
CREATE INDEX idx_usage_user_time ON cortex_auth.usage (user_id, occurred_at DESC);
