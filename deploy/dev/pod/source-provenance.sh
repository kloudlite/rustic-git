#!/usr/bin/env bash

source_provenance_head() {
  git -C "$1" rev-parse HEAD
}

source_provenance_require_clean() {
  local root=$1 status
  status=$(git -C "$root" status --porcelain --untracked-files=all) || {
    echo "could not inspect source tree at $root" >&2
    return 2
  }
  if [ -n "$status" ]; then
    echo "source tree is dirty; commit tracked, staged, and untracked changes first" >&2
    return 2
  fi
}

source_provenance_capture() {
  local root=$1 head
  head=$(source_provenance_head "$root") || return 2
  source_provenance_require_clean "$root" || return 2
  printf '%s\n' "$head"
}

source_provenance_verify() {
  local root=$1 expected=$2 head
  head=$(source_provenance_head "$root") || return 2
  if [ "$head" != "$expected" ]; then
    echo "source HEAD changed from $expected to $head" >&2
    return 2
  fi
  source_provenance_require_clean "$root" || return 2
}

source_provenance_write_record() {
  local record=$1 tmp=$2 sha=$3
  mkdir -p "$(dirname "$record")" || return 2
  printf '%s\n' "$sha" > "$tmp" || return 2
  mv "$tmp" "$record"
}
