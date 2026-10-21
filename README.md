# lumenfx-bot

The lumen-fx maintainer bot: a GitHub App that triages pull requests and issues, keeps their status labels current, and tells contributors what is left before review.

## Why

The project has one maintainer who reviews in one sitting a day. Between sittings, contributors need to know whether a red check is theirs, whether the CLA is signed, and whether anyone is waiting on them. The bot answers that on every pull request, from GitHub's current state and the repository's `.github/lumenfx.toml`, with no model in the decision path: the same state always produces the same labels and the same comment.

## Quick start

Build and run the tests (the replay and receiver suites need a Postgres the test user may create databases on):

```sh
docker run -d --name lumenfx-pg -e POSTGRES_USER=bot -e POSTGRES_PASSWORD=bot -p 55432:5432 postgres:17-alpine
export DATABASE_URL=postgres://bot:bot@localhost:55432/bot
cargo test --workspace
```

Check a config file, or see what the bot would do on a live pull request without writing anything:

```sh
cargo run --bin lumenfx-bot -- check-config examples/lumenfx.toml
GITHUB_TOKEN=$(gh auth token) cargo run --bin lumenfx-bot -- plan lumen-fx/lumen 562 --config examples/lumenfx.toml
```

To run it for the organization:

1. Register the App from `deploy/github-app/manifest.json` with `deploy/github-app/register.html`, and exchange the code GitHub returns with `gh api -X POST /app-manifests/CODE/conversions`.
2. Create the two Secrets named in `deploy/k8s/kustomization.yaml` and apply it with `kubectl apply -k deploy/k8s`. The bot starts in dry run.
3. Install the App on the repositories it should serve and commit a `.github/lumenfx.toml` to each; `examples/lumenfx.toml` is the one for lumen-fx/lumen.

## What it does

- Keeps one of three status labels on every open pull request: needs triage, waiting on author, or needs review. It decides from the whole pull request each time: CLA, conflicts, merge commits, required checks, unaddressed change requests, drafts, and open concerns. Labels that a maintainer set, such as blocked or needs design, freeze it.
- Adds an area label from the changed paths and a class label from the conventional-commit title when the pull request has none.
- Posts one status comment when the author has something to do, a fork run waits for approval, or the author is new, and edits that comment from then on. It lists what is left, the required checks, the Codecov patch coverage, Pullsift's screening, and a docs reminder when code changed and no doc did.
- Posts a separate comment only when someone needs a notification: a reply to a command, a new merge conflict, or one nudge after the author has been quiet for two weeks. No thread gets more than three new comments a day.
- Answers commands in comments, each written as `@lumenfx-bot <command>` on its own line: `help`, `ready`, `author`, `blocked`, `unblock`, `label`, `concern`, `resolve`, and `no-docs-needed`. A bare word without the mention does nothing. The full list is on the bot's `/help` page.
- Checks a pull request that edits `.github/lumenfx.toml` and reports the errors of the edited file as a `lumenfx config` check, so a broken config is caught before it merges.
- Runs in dry run (`LUMENFX_DRY_RUN=1`): every plan is logged and recorded in the audit table, and nothing is written to GitHub.

## Limitations

The CLA is still signed through the existing CLA workflow; the bot reads its `cla` check. Issues get commands but no triage labels yet. Merge-queue handling, flaky-test tracking, the CodeQL gate, first-time CI approval, and release commits are planned and not built yet; their commands answer "not available yet". A repository without `.github/lumenfx.toml` gets answers to `help` and nothing else.

## More

- `examples/lumenfx.toml`: every config section with the values lumen-fx/lumen uses.
- `/help` and `/status` on the running bot: the command list, and the queue, timers, rate limit, and config state per repository.
- `crates/server/tests/fixtures`: replay scenarios for each rule and command, with the exact comments and labels the bot writes.
