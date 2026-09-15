#!/usr/bin/env bash
#
# Re-derive `contracts/tool-proxy-requests.json` from nucleus's own source and
# diff it against what this repo believes.
#
# The bridge's correctness depends on field names in another repository. The unit
# test `the_translation_emits_only_fields_the_route_deserialises` checks that the
# translation agrees with the contract file; nothing in this repo can check that
# the contract file still agrees with *nucleus*. This does, by reading the
# `#[derive(Deserialize)]` structs at the commit the contract names.
#
# It fetches, so it is a separate CI job from the mediation law — that one decides
# from source alone and must keep working with no network.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
contract="$here/contracts/tool-proxy-requests.json"

repo=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['_nucleus_repo'])" "$contract")
commit=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['_nucleus_commit'])" "$contract")
source_path=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['_nucleus_source'])" "$contract")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "Reading $repo at $commit"
git init -q "$work/nucleus"
git -C "$work/nucleus" remote add origin "$repo"
# A blobless partial clone: the history is large and one file is wanted.
git -C "$work/nucleus" fetch -q --depth 1 --filter=blob:none origin "$commit"
git -C "$work/nucleus" checkout -q FETCH_HEAD -- "$source_path"

python3 "$here/ci/derive_proxy_contract.py" \
  "$work/nucleus/$source_path" "$contract"
