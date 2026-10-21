-- Every webhook delivery, as received. Redeliveries share a delivery ID and
-- are stored once. Synthetic events (timers, sweeps, fan-out from a commit
-- to its pull requests) use IDs with a prefix GitHub never sends.
CREATE TABLE events (
    delivery_id     TEXT PRIMARY KEY,
    event           TEXT NOT NULL,
    action          TEXT,
    repo            TEXT,
    number          BIGINT,
    installation_id BIGINT,
    payload         JSONB NOT NULL,
    received_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at    TIMESTAMPTZ,
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until    TIMESTAMPTZ,
    error           TEXT
);
CREATE INDEX events_pending ON events (received_at) WHERE processed_at IS NULL;
CREATE INDEX events_entity ON events (repo, number, event, action);

-- Per-thread state the bot keeps (triagebot's issue_data).
CREATE TABLE entity_data (
    repo       TEXT NOT NULL,
    number     BIGINT NOT NULL,
    key        TEXT NOT NULL,
    data       JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo, number, key)
);

-- Comments the bot posted, by purpose; the status comment has purpose
-- 'status'. Also the source of the per-thread daily cap.
CREATE TABLE bot_comments (
    repo        TEXT NOT NULL,
    number      BIGINT NOT NULL,
    purpose     TEXT NOT NULL,
    key         TEXT NOT NULL,
    comment_id  BIGINT NOT NULL,
    body_sha256 TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo, number, purpose, key)
);

-- Labels the bot added, so it removes only its own.
CREATE TABLE owned_labels (
    repo     TEXT NOT NULL,
    number   BIGINT NOT NULL,
    label    TEXT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo, number, label)
);

-- Timers. They survive restarts; the same (name, time) is one job.
CREATE TABLE jobs (
    id           BIGSERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    scheduled_at TIMESTAMPTZ NOT NULL,
    metadata     JSONB NOT NULL DEFAULT '{}',
    executed_at  TIMESTAMPTZ,
    error        TEXT,
    UNIQUE (name, scheduled_at)
);
CREATE INDEX jobs_due ON jobs (scheduled_at) WHERE executed_at IS NULL;

-- Every write the bot made or, in dry run, would have made, with its reason.
CREATE TABLE actions_log (
    id      BIGSERIAL PRIMARY KEY,
    at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    repo    TEXT NOT NULL,
    number  BIGINT NOT NULL,
    action  TEXT NOT NULL,
    target  TEXT NOT NULL,
    rule    TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    dry_run BOOLEAN NOT NULL
);
CREATE INDEX actions_log_thread ON actions_log (repo, number, at);

-- The config file last read from each repository's default branch, and the
-- last version that validated.
CREATE TABLE configs (
    repo           TEXT PRIMARY KEY,
    blob_sha       TEXT NOT NULL,
    text           TEXT NOT NULL,
    valid          BOOLEAN NOT NULL,
    errors         JSONB NOT NULL DEFAULT '[]',
    last_good_text TEXT,
    checked_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE meta (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
