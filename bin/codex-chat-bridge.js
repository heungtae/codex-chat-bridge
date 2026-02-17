#!/usr/bin/env node

const { existsSync } = require("node:fs");
const { spawnSync } = require("node:child_process");
const { resolve } = require("node:path");

const root = resolve(__dirname, "..");
const releaseBin = resolve(root, "target", "release", "codex-chat-bridge");
const debugBin = resolve(root, "target", "debug", "codex-chat-bridge");

const binPath = existsSync(releaseBin)
  ? releaseBin
  : existsSync(debugBin)
    ? debugBin
    : null;

if (!binPath) {
  console.error("error: codex-chat-bridge binary not found.");
  console.error("run `npm run build:bridge` to compile the Rust binary.");
  process.exit(1);
}

const child = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });

if (child.error) {
  console.error(`error: failed to start bridge binary: ${child.error.message}`);
  process.exit(1);
}

if (typeof child.status === "number") {
  process.exit(child.status);
}

process.exit(1);
