-- A cache of the CLA signature store. The file in the store repository is
-- the source of truth; these rows mirror the blob last read or written.
CREATE TABLE cla_store (
    store    TEXT NOT NULL,
    path     TEXT NOT NULL,
    -- Empty while the file does not exist yet.
    blob_sha TEXT NOT NULL,
    read_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store, path)
);

CREATE TABLE cla_signers (
    store            TEXT NOT NULL,
    path             TEXT NOT NULL,
    github_id        BIGINT NOT NULL,
    login_at_signing TEXT NOT NULL,
    signed_at        TEXT NOT NULL,
    comment_id       BIGINT,
    store_sha        TEXT NOT NULL,
    PRIMARY KEY (store, path, github_id)
);

-- Commit and Co-authored-by emails resolved to GitHub accounts; a row with
-- no account records a lookup that found none.
CREATE TABLE email_accounts (
    email       TEXT PRIMARY KEY,
    account_id  BIGINT,
    login       TEXT,
    is_bot      BOOLEAN NOT NULL DEFAULT false,
    resolved_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
