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

function getLatestPublishedVersion() {
  const result = spawnSync("npm", ["view", packageName, "version", "--json"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 5000,
  });

  if (result.error || result.status !== 0 || !result.stdout.trim()) {
    return null;
  }
  try {
    const parsed = JSON.parse(result.stdout);
    if (typeof parsed === "string" && parsed.trim()) {
      return parsed.trim();
    }
    if (parsed && typeof parsed.version === "string" && parsed.version.trim()) {
      return parsed.version.trim();
    }
    if (Array.isArray(parsed) && typeof parsed[0] === "string") {
      return parsed[0].trim();
    }
    return null;
  } catch {
    const raw = result.stdout.trim();
    if (!raw) {
      return null;
    }
    return raw.replace(/^"+|"+$/g, "");
  }
}

function normalizeSemver(version) {
  const match = String(version).trim().match(/^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/);
  if (!match) {
    return null;
  }
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

function isNewerVersion(currentVersion, latestVersion) {
  const current = normalizeSemver(currentVersion);
  const latest = normalizeSemver(latestVersion);
  if (!current || !latest) {
    return latestVersion !== currentVersion;
  }
  for (let i = 0; i < 3; i += 1) {
    if (latest[i] > current[i]) {
      return true;
    }
    if (latest[i] < current[i]) {
      return false;
    }
  }
  return false;
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
  console.error(`info: running update: npm ${updateArgs.join(" ")}`);
  const updateResult = spawnSync("npm", updateArgs, { stdio: "inherit", timeout: 60000 });
  if (updateResult.error || updateResult.status !== 0) {
    console.error("error: update failed.");
    return false;
  }
  console.error(`info: update completed. Restart to use ${latest}.`);
  return true;
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
  console.error(`  1. Update now and exit (runs \`npm install -g ${packageName}\`)`);
  console.error("  2. Skip this run");
  console.error("  3. Skip this version");
  console.error("");

  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stderr,
  });

  try {
    const answer = (await askQuestion(rl, "  Choose [1/2/3] (default: 2): ")).trim();
    if (answer === "" || answer === "2") {
      return "skip";
    }
    if (answer === "1") {
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

  const latest = getLatestPublishedVersion();
  if (!latest || !isNewerVersion(packageVersion, latest)) {
    return;
  }

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
    console.error("info: non-interactive mode detected; skipping update prompt.");
    return;
  }

  const choice = await promptUpdateChoice(packageVersion, latest);
  if (choice === "update") {
    const ok = runUpdateCommand(updateArgs, latest);
    process.exit(ok ? 0 : 1);
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
