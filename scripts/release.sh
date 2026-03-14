#!/bin/bash
set -e

VERSION=$1
if [ -z "$VERSION" ]; then
  echo "Usage: ./scripts/release.sh 0.3.0"
  exit 1
fi

echo "=== Generating docs ==="
npx @cueframe/autodocs generate

echo "=== Deploying docs ==="
npx @cueframe/autodocs deploy

echo "=== Updating version ==="
sed -i '' "s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml

echo "=== Committing ==="
git add Cargo.toml docs/
git diff --cached --quiet || git commit -m "v${VERSION}"

echo "=== Tagging and pushing ==="
git tag "v${VERSION}"
git push origin main --tags

echo "=== Done ==="
