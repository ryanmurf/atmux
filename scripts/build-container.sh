#!/usr/bin/env bash
set -euo pipefail

repository="${1:-localhost:32000/atmux}"
if [[ ! "$repository" =~ ^[A-Za-z0-9._/:-]+$ \
  || "$repository" == -* \
  || "${repository##*/}" == *:* ]]; then
  echo "container repository must be a nonempty Docker repository without a tag" >&2
  exit 2
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"
commit="$(git rev-parse --verify 'HEAD^{commit}')"

# The image and its provenance label must describe the same committed inputs.
# Source edits elsewhere are harmless because git archive excludes them, but a
# dirty or missing build definition could otherwise make this helper lie about
# what Docker evaluated.
required=(Dockerfile .dockerignore Cargo.toml Cargo.lock scripts/build-container.sh)
for path in "${required[@]}"; do
  if ! git cat-file -e "HEAD:${path}" 2>/dev/null; then
    echo "required container input is not committed at HEAD: ${path}" >&2
    exit 1
  fi
done
if ! git diff --quiet HEAD -- "${required[@]}"; then
  echo "required container inputs differ from HEAD; commit them before building" >&2
  exit 1
fi

image="${repository}:${commit:0:12}"
git archive --format=tar HEAD | docker build \
  --no-cache \
  --build-arg "VCS_REF=${commit}" \
  --tag "$image" \
  -

printf 'built %s from commit %s\n' "$image" "$commit"
