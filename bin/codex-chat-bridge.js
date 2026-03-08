#!/usr/bin/env node

const { existsSync, mkdirSync, readFileSync, writeFileSync } = require("node:fs");
const { spawnSync } = require("node:child_process");
const { resolve } = require("node:path");
const { homedir } = require("node:os");
const readline = require("node:readline");

const root = resolve(__dirname, "..");
const packageName = "@heungtae/codex-chat-bridge";
const { version: packageVersion } = require(resolve(root, "package.json"));
const releaseBin = resolve(root, "target", "release", "codex-chat-bridge");
const debugBin = resolve(root, "target", "debug", "codex-chat-bridge");
const updatePrefPath = resolve(
  homedir(),
  ".config",
  "codex-chat-bridge",
  "update-preferences.json"
);

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

function loadUpdatePreference() {
  if (!existsSync(updatePrefPath)) {
    return {};
  }
  try {
    const parsed = JSON.parse(readFileSync(updatePrefPath, "utf8"));
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function saveUpdatePreference(pref) {
  try {
    mkdirSync(resolve(updatePrefPath, ".."), { recursive: true });
    writeFileSync(updatePrefPath, `${JSON.stringify(pref, null, 2)}\n`);
  } catch {
    // Ignore preference save failures and continue runtime.
  }
}

function runUpdateCommand(updateArgs, latest) {
  console.error(`info: running automatic update: npm ${updateArgs.join(" ")}`);
  const updateResult = spawnSync("npm", updateArgs, { stdio: "inherit", timeout: 60000 });
  if (updateResult.error || updateResult.status !== 0) {
    console.error("warn: automatic update failed. Continue with current installed version.");
  } else {
    console.error(`info: update completed. Restart to use ${latest}.`);
  }
}

function askQuestion(rl, prompt) {
  return new Promise((resolveAnswer) => {
    rl.question(prompt, (answer) => resolveAnswer(answer));
  });
}

async function promptUpdateChoice(currentVersion, latestVersion) {
  console.error("");
  console.error(`✨ Update available! ${currentVersion} -> ${latestVersion}`);
  console.error("");
  console.error("  Release notes: https://github.com/openai/codex/releases/latest");
  console.error("");
  console.error(`› 1. Update now (runs \`npm install -g ${packageName}\`)`);
  console.error("  2. Skip");
  console.error("  3. Skip until next version");
  console.error("");

  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stderr,
  });

  try {
    const answer = (await askQuestion(rl, "  Press enter to continue ")).trim();
    if (answer === "" || answer === "1") {
      return "update";
    }
    if (answer === "3") {
      return "skip-until-next-version";
    }
    return "skip";
  } finally {
    rl.close();
  }
}

async function autoUpdateIfOutdated() {
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

  const pref = loadUpdatePreference();
  if (pref.skipUntilVersion === latest) {
    return;
  }

  if (!process.stdin.isTTY || !process.stderr.isTTY) {
    runUpdateCommand(updateArgs, latest);
    return;
  }

  const choice = await promptUpdateChoice(packageVersion, latest);
  if (choice === "update") {
    runUpdateCommand(updateArgs, latest);
    return;
  }
  if (choice === "skip-until-next-version") {
    saveUpdatePreference({ skipUntilVersion: latest });
  }
}

async function main() {
  await autoUpdateIfOutdated();

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
}

main().catch((err) => {
  console.error(`error: launcher failed: ${err.message}`);
  process.exit(1);
});
