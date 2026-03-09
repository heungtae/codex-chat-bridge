#!/usr/bin/env bash
set -euo pipefail

PACKAGE_NAME="@heungtae/codex-chat-bridge"

if ! command -v node >/dev/null 2>&1; then
  echo "error: node is required" >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "error: npm is required" >&2
  exit 1
fi

if [[ ! -f package.json ]]; then
  echo "error: package.json not found in current directory" >&2
  exit 1
fi

if [[ ! -f Cargo.toml ]]; then
  echo "error: Cargo.toml not found in current directory" >&2
  exit 1
fi

pkg_version="$(node -p "require('./package.json').version")"
cargo_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
npm_latest="$(npm view "$PACKAGE_NAME" version 2>/dev/null || true)"

local_installed="$(npm ls "$PACKAGE_NAME" --depth=0 --json 2>/dev/null | node -e "let s=''; process.stdin.on('data',d=>s+=d).on('end',()=>{try{const j=JSON.parse(s||'{}'); const v=j.dependencies && j.dependencies['$PACKAGE_NAME'] && j.dependencies['$PACKAGE_NAME'].version; process.stdout.write(v||'not-installed');}catch{process.stdout.write('not-installed')}})")"

global_installed="$(npm ls -g "$PACKAGE_NAME" --depth=0 --json 2>/dev/null | node -e "let s=''; process.stdin.on('data',d=>s+=d).on('end',()=>{try{const j=JSON.parse(s||'{}'); const v=j.dependencies && j.dependencies['$PACKAGE_NAME'] && j.dependencies['$PACKAGE_NAME'].version; process.stdout.write(v||'not-installed');}catch{process.stdout.write('not-installed')}})")"

echo "== codex-chat-bridge version check =="
echo "package.json        : $pkg_version"
echo "Cargo.toml          : ${cargo_version:-unknown}"
echo "npm latest          : ${npm_latest:-unknown}"
echo "npm local installed : $local_installed"
echo "npm global installed: $global_installed"

if [[ -n "${npm_latest:-}" && "$pkg_version" == "$npm_latest" ]]; then
  echo "status: local package version matches npm latest"
elif [[ -n "${npm_latest:-}" ]]; then
  echo "status: update available or local is different (local=$pkg_version, latest=$npm_latest)"
else
  echo "status: could not fetch npm latest version"
fi
