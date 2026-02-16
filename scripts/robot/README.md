# Robot Script Catalog

This repo exposes a minimal script catalog consumed by `robot`.

## Files

- `manifest.json` defines script shortcuts and risk levels.

## Usage

```bash
robot scripts list
robot scripts run scripts.inventory
robot scripts run scripts.sample --force
```

## Catalog Policy

- `scripts.inventory` is read-only and safe.
- `scripts.sample` is repo-specific and marked `ask-first`.
- Add additional commands only when they are stable and clearly documented.
