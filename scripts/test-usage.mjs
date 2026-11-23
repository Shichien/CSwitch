import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import ts from "typescript";

const source = readFileSync(new URL("../src/lib/usage.ts", import.meta.url), "utf8");
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } });
const { mergeOfficialUsage, mergeProviderUsage, updatedLabel, resetTimeLabel } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}`);
const old = { health: "valid", windows: [{ usedPercent: 80 }], queriedAt: 1000 };
const previous = { checking: true, data: old };
const failed = mergeOfficialUsage(previous, { health: "unknown", message: "网络超时", queriedAt: 2000 });
assert.equal(failed.data, old);
assert.equal(failed.refreshError, "网络超时");
assert.equal(failed.checking, false);
assert.equal(mergeOfficialUsage({ ...failed, checking: true }, { health: "unknown" }).data, old);
const recovered = { health: "valid", queriedAt: 3000, windows: [] };
assert.deepEqual(mergeOfficialUsage(failed, recovered), { checking: false, data: recovered });
const expired = { health: "reauth_required" };
assert.equal(mergeOfficialUsage(previous, expired).data, expired);
const firstFailure = { health: "unknown" };
assert.equal(mergeOfficialUsage(undefined, firstFailure).data, firstFailure);
const balance = { status: "available", balance: 12, queriedAt: 1000 };
assert.equal(mergeProviderUsage({ data: balance }, { status: "unknown" }).data, balance);
for (const status of ["unauthorized", "unsupported"]) {
  assert.equal(mergeProviderUsage({ data: balance }, { status }).data.status, status);
}
assert.equal(updatedLabel(1000, 301000), "5 分钟前更新");
assert.equal(updatedLabel(1000, 1000), "刚刚更新");
assert.equal(updatedLabel(2000, 1000), "刚刚更新");
assert.equal(updatedLabel(0, 7200000), "2 小时前更新");
assert.equal(updatedLabel(0, 172800000), "2 天前更新");
const now = Date.UTC(2026, 10, 23);
assert.match(resetTimeLabel((now + 135 * 60000) / 1000, now), /^还有 2 小时 15 分钟重置/);
assert.match(resetTimeLabel((now + 30000) / 1000, now), /^还有 1 分钟重置/);
assert.match(resetTimeLabel((now - 1000) / 1000, now), /^已到重置时间，请刷新额度/);
assert.equal(resetTimeLabel(null, now), null);
assert.equal(resetTimeLabel(NaN, now), null);
console.log("Usage state and relative-time checks passed.");
