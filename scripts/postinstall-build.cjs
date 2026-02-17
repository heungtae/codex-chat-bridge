#!/usr/bin/env node

const { spawnSync } = require("node:child_process");

const result = spawnSync(
  "cargo",
  ["build", "--release", "--bin", "codex-chat-bridge"],
  { stdio: "inherit" }
);

if (result.error) {
  console.error("error: failed to execute `cargo build` during npm postinstall.");
  console.error("install Rust/Cargo first: https://www.rust-lang.org/tools/install");
  console.error(`detail: ${result.error.message}`);
  process.exit(1);
}

if (result.status !== 0) {
  console.error("error: cargo build failed during npm postinstall.");
  process.exit(result.status ?? 1);
}
