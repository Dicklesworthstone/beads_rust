#!/usr/bin/env bash
# Verify the release workflow's high-risk shell fragments.

set -euo pipefail

cargo test --test workflow_release_fragments -- --nocapture
