# lumenfx-bot

GitHub App that triages lumen-fx pull requests and issues: status labels, area labels, the CLA and CI status comment, and `@lumenfx-bot` commands.

Config lives in each repo's `.github/lumenfx.toml`; see `examples/lumenfx.toml`. Commands are listed on the bot's `/help` page.

A repo with `[cla] enabled = true` gets the `cla` check on pull requests and merge groups. Contributors sign with `@lumenfx-bot sign <sentence>`; signatures go to the store repo the config names, so the App needs contents write there.

## Development

```sh
docker run -d --name lumenfx-pg -e POSTGRES_USER=bot -e POSTGRES_PASSWORD=bot -p 55432:5432 postgres:17-alpine
export DATABASE_URL=postgres://bot:bot@localhost:55432/bot
cargo test --workspace
cargo run --bin lumenfx-bot -- check-config examples/lumenfx.toml
GITHUB_TOKEN=$(gh auth token) cargo run --bin lumenfx-bot -- plan lumen-fx/lumen 562 --config examples/lumenfx.toml
```

## Deploy

The image is `ghcr.io/lumen-fx/bot:main`, built by `release.yml`. It runs on microk8s from `deploy/k8s` (secrets listed in `kustomization.yaml`) and starts in dry run. The App is registered from `deploy/github-app/manifest.json`.
