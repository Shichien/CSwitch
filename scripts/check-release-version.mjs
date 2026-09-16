import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
const pkg = JSON.parse(readFileSync("package.json", "utf8"));
const tauri = JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8"));
const cargo = readFileSync("src-tauri/Cargo.toml", "utf8").match(/^version\s*=\s*"([^"]+)"/m)?.[1];
assert.equal(pkg.version, tauri.version, "前端与安装包版本不一致");
assert.equal(pkg.version, cargo, "前端与 Rust 版本不一致");
if (process.env.GITHUB_REF_TYPE === "tag") {
  assert.equal(process.env.GITHUB_REF_NAME, `v${pkg.version}`, "发布标签与应用版本不一致");
}
console.log(`CSwitch ${pkg.version}: version check passed`);
