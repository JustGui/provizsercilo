#!/usr/bin/env bash
# Install git hooks for ProvizSercilo.
set -euo pipefail

HOOKS_SRC="$(cd "$(dirname "$0")/hooks" && pwd)"
HOOKS_DEST="$(git rev-parse --git-dir)/hooks"

for hook in "$HOOKS_SRC"/*; do
    name="$(basename "$hook")"
    dest="$HOOKS_DEST/$name"
    cp "$hook" "$dest"
    chmod +x "$dest"
    echo "installed: $dest"
done

echo "git hooks installed successfully."
