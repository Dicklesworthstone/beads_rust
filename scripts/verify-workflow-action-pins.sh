#!/usr/bin/env bash
# Verify that external GitHub Actions references stay SHA-pinned and inventoried.

set -euo pipefail

cargo test --test workflow_action_pins -- --nocapture
