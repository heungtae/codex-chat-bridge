#!/usr/bin/env node

const { existsSync, readFileSync, writeFileSync } = require("node:fs");
const { resolve } = require("node:path");

const root = resolve(__dirname, "..");
const cargoTomlPath = resolve(root, "Cargo.toml");
const packageJsonPath = resolve(root, "package.json");

function readCargoVersion(cargoToml) {
  const lines = cargoToml.split(/\r?\n/);
  let inPackageSection = false;

  for (const line of lines) {
    const trimmed = line.trim();

    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      inPackageSection = trimmed === "[package]";
      continue;
    }

    if (!inPackageSection) {
      continue;
    }

    const versionMatch = line.match(/^\s*version\s*=\s*"([^"]+)"/);
    if (versionMatch) {
      return versionMatch[1];
    }
  }

  throw new Error("missing package version in Cargo.toml");
}

function main() {
  if (!existsSync(cargoTomlPath) || !existsSync(packageJsonPath)) {
    throw new Error("Cargo.toml or package.json not found");
  }

  const cargoToml = readFileSync(cargoTomlPath, "utf8");
  const cargoVersion = readCargoVersion(cargoToml);

  const packageJson = JSON.parse(readFileSync(packageJsonPath, "utf8"));
  if (packageJson.version === cargoVersion) {
    return;
  }

  packageJson.version = cargoVersion;
  writeFileSync(packageJsonPath, `${JSON.stringify(packageJson, null, 2)}\n`);
  console.log(`synced package.json version to ${cargoVersion}`);
}

main();
