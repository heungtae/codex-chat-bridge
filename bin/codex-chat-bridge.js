#!/usr/bin/env node

const { existsSync } = require("node:fs");
const { spawnSync } = require("node:child_process");
const { resolve } = require("node:path");

const root = resolve(__dirname, "..");
const packageName = "@heungtae/codex-chat-bridge";
const { version: packageVersion } = require(resolve(root, "package.json"));
const releaseBin = resolve(root, "target", "release", "codex-chat-bridge");
const debugBin = resolve(root, "target", "debug", "codex-chat-bridge");

function getOutdatedPackageInfo() {
  const result = spawnSync("npm", ["outdated", "--json", packageName], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 5000,
  });

  if (result.error || !result.stdout.trim()) {
    return null;
  }

  if (result.status !== 1) {
    return null;
  }

  try {
    const parsed = JSON.parse(result.stdout);
    const info = parsed[packageName];
    if (!info || !info.latest) {
      return null;
    }
    return info;
  } catch {
    return null;
  }
}

function isGlobalInstallPath() {
  const result = spawnSync("npm", ["root", "-g"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 3000,
  });

  if (result.status !== 0 || !result.stdout.trim()) {
    return false;
  }

  const globalRoot = result.stdout.trim();
  return root.startsWith(globalRoot);
}

function autoUpdateIfOutdated() {
  if (process.env.CODEX_CHAT_BRIDGE_AUTO_UPDATE === "0") {
    return;
  }

  const outdated = getOutdatedPackageInfo();
  if (!outdated) {
    return;
  }

  const latest = outdated.latest;
  console.error(
    `warn: ${packageName} has a newer version (${packageVersion} -> ${latest}).`
  );
  console.error(`warn: recommended command: npm install -g ${packageName}@latest`);

  const updateArgs = isGlobalInstallPath()
    ? ["install", "-g", `${packageName}@latest`]
    : ["install", `${packageName}@latest`];

  console.error(`info: running automatic update: npm ${updateArgs.join(" ")}`);
  const updateResult = spawnSync("npm", updateArgs, { stdio: "inherit", timeout: 60000 });

  if (updateResult.error || updateResult.status !== 0) {
    console.error("warn: automatic update failed. Continue with current installed version.");
  } else {
    console.error(`info: update completed. Restart to use ${latest}.`);
  }
}

autoUpdateIfOutdated();

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
