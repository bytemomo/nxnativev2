# nxnativev2's everyday commands: `just` lists them. Secrets come from 1Password (op) when a recipe needs them, never from a file:
# the release key is passed as its reference (xtask reads it with `op read`), the GitHub token is read only for the command that uses it.

set shell := ["bash", "-euo", "pipefail", "-c"]

repo := "bytemomo/nxnativev2"
github_token := "op://nxnativev2/nxnativev2 github token/credential"
release_key := "op://nxnativev2/nxnativev2 release key/credential"

# the recipes
default:
    @just --list --unsorted

# ---------- builds ----------

# a development build (debug): target/debug/nxnativev2 and nxnative, no bundle (nxnative_repo / python in the settings)
build:
    cargo build -p nxapp -p nxcli

# a development build (optimized): target/release/nxnativev2 and nxnative
build-release:
    cargo build --release -p nxapp -p nxcli

# the packaged app and CLI with their bundle, for one platform (linux or windows): dist/
dist target="linux":
    cargo xtask bundle --target {{target}} --cli

# the packaged app and CLI for Linux and Windows
dist-all: (dist "linux") (dist "windows")

# type-check and test the workspace, and the local play core
check:
    cargo check --workspace
    cargo test --workspace
    runtime/lan/tests/run.sh

# ---------- releases ----------

# the release key in 1Password matches the one clients trust (nothing is built)
keycheck:
    NXNATIVEV2_SIGNING_KEY='{{release_key}}' cargo xtask keycheck

# what the next release would build, and why: each component's inputs against the latest release (nothing is built)
plan:
    cargo xtask release --plan

# both platforms' release into dist/release/ (apps, bundles, SBOMs, the signed manifest), not published
release:
    NXNATIVEV2_SIGNING_KEY='{{release_key}}' cargo xtask release

# build, sign and publish a release on GitHub with what changed since the latest one (chosen from the inputs): clients see it as newer and update
publish:
    NXNATIVEV2_SIGNING_KEY='{{release_key}}' cargo xtask keycheck
    GH_TOKEN="$(op read '{{github_token}}')" NXNATIVEV2_SIGNING_KEY='{{release_key}}' cargo xtask release --publish

# publish what dist/release/ holds already (a release built before, whose upload failed): its tag is the manifest's
upload:
    #!/usr/bin/env bash
    set -euo pipefail
    tag=$(python3 -c "import json; print(json.load(open('dist/release/manifest.json'))['tag'])")
    commit=$(python3 -c "import json; print(json.load(open('dist/release/manifest.json'))['commit'])")
    echo "publishing $tag (commit ${commit:0:12}) from dist/release/"
    GH_TOKEN="$(op read '{{github_token}}')" gh release create "$tag" --repo {{repo}} --title "nxnativev2 $tag" \
        --notes "Commit $commit. The app checks the signed manifest.json before it installs anything; each bundle's SBOM is bundle-<platform>.cdx.json." \
        --latest --target "$commit" dist/release/*

# ---------- CI ----------

# the release key as the repository secret CI signs with (NXNATIVEV2_SIGNING_KEY), straight from 1Password
ci-secret:
    op read '{{release_key}}' | GH_TOKEN="$(op read '{{github_token}}')" gh secret set NXNATIVEV2_SIGNING_KEY --repo {{repo}}

# a release made in GitHub Actions now (as a [ci] commit on main does)
ci-release:
    GH_TOKEN="$(op read '{{github_token}}')" gh workflow run release.yml --repo {{repo}} --ref main

# the latest CI runs of the release workflow
ci-status:
    GH_TOKEN="$(op read '{{github_token}}')" gh run list --workflow release.yml --repo {{repo}} --limit 5
