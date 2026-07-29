#!/usr/bin/env bash
# Verify the notify-ACFS workflow's local contract without network calls.

set -euo pipefail

cargo test --test workflow_notify_acfs -- --nocapture
