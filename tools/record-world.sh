#!/usr/bin/env bash
# Record the GitHub REST and GraphQL documents the bot reads for one pull
# request into a replay "world" file. Read-only: it only issues GETs and one
# GraphQL query, with whatever `gh` is logged in as.
#
#   tools/record-world.sh lumen-fx/lumen 562 > crates/server/tests/fixtures/worlds/lumen-562.json
set -euo pipefail

repo=${1:?usage: record-world.sh OWNER/REPO NUMBER}
n=${2:?usage: record-world.sh OWNER/REPO NUMBER}
owner=${repo%/*}
name=${repo#*/}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# put KEY: store stdin as the document served at KEY.
put() { cat > "$tmp/$(printf '%s' "$1" | sha256sum | cut -c1-16).json"; printf '%s\t%s\n' "$1" "$(printf '%s' "$1" | sha256sum | cut -c1-16)" >> "$tmp/index"; }
get() { gh api "$1"; }
all() { gh api --paginate --slurp "$1" | jq 'add // []'; }
all_field() { gh api --paginate --slurp "$1" | jq --arg f "$2" '{($f): (map(.[$f]) | add // [])}'; }

get "repos/$repo/pulls/$n" > "$tmp/pull"
sha=$(jq -r .head.sha "$tmp/pull")
base=$(jq -r .base.ref "$tmp/pull")
author=$(jq -r .user.login "$tmp/pull")
p="/repos/$repo"

get "repos/$repo" | put "$p"
get "repos/$repo/issues/$n" | put "$p/issues/$n"
all "repos/$repo/issues/$n/comments?per_page=100" | put "$p/issues/$n/comments"
put "$p/pulls/$n" < "$tmp/pull"
all "repos/$repo/pulls/$n/files?per_page=100" | put "$p/pulls/$n/files"
all "repos/$repo/pulls/$n/commits?per_page=100" | put "$p/pulls/$n/commits"
get "repos/$repo/rules/branches/$base" | put "$p/rules/branches/$base"
all_field "repos/$repo/commits/$sha/check-runs?filter=latest&per_page=100" check_runs | put "$p/commits/$sha/check-runs"
get "repos/$repo/commits/$sha/status" | put "$p/commits/$sha/status"
all "repos/$repo/pulls/$n/reviews?per_page=100" | put "$p/pulls/$n/reviews"
all "repos/$repo/issues/$n/timeline?per_page=100" | put "$p/issues/$n/timeline"
all "repos/$repo/labels?per_page=100" | put "$p/labels"
get "repos/$repo/collaborators/$author/permission" | put "$p/collaborators/$author/permission"

query=$(sed -n '/^pub const PULL_STATE_QUERY: &str = r#"/,/^}"#;/p' "$(dirname "$0")/../crates/server/src/snapshot.rs" \
  | sed '1s/^pub const PULL_STATE_QUERY: &str = r#"//; $s/"#;$//')
gh api graphql -f query="$query" -F owner="$owner" -F name="$name" -F number="$n" | jq .data > "$tmp/gql"

{
  printf '{"docs":{'
  first=1
  while IFS=$'\t' read -r key file; do
    [ $first -eq 1 ] || printf ','
    first=0
    jq -n --arg k "$key" '$k' | tr -d '\n'
    printf ':'
    jq -c . "$tmp/$file.json"
  done < "$tmp/index"
  printf '},"graphql":{"PullState#%s":' "$n"
  jq -c . "$tmp/gql"
  printf '}}\n'
} | jq .
