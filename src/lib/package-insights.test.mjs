import test from "node:test";
import assert from "node:assert/strict";
import { visibleGraph, nodeColor } from "./dependency-graph.ts";
import {
  DependencyJobSession, DependencyReviewSession, PackageInsightsQueue, formatCount, githubProblemText, githubStatusText, groupFindings, newestText,
  packageMajorLag, packagePage, parseNumericMajor, projectVersionText, rankPackages,
  rankingIncomplete, safePublicUrl,
} from "./package-insights.ts";

const node = (id, extra = {}) => ({ id, name: id, version: "legacy-guess", resolvedVersions: [], declaredVersions: [], ecosystem: id === "project" ? "project" : "npm", direct: false, dev: false, ...extra });
const metric = (value, status = value === null ? "unavailable" : "available") => ({ value, status, source: "GitHub", period: null, reason: null, retryAt: null });
const info = (id, stars = null, contributors = null, extra = {}) => ({
  id, registryUrl: "https://www.npmjs.com/package/example", newestVersion: "4.0.0", newestStatus: "available",
  repository: { url: "https://github.com/example/repo", ownerLogin: "example", ownerType: "organization", stars: metric(stars), contributors: metric(contributors), checkedAt: 1 },
  repositoryReason: null, description: "Example package.", repositoryUrl: "https://github.com/example/repo", githubPending: false, downloads: metric(null), checkedAt: 1, notes: [], ...extra,
});
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = async () => { for (let index = 0; index < 8; index++) await Promise.resolve(); };

class Clock {
  time = 0;
  nextId = 0;
  timers = new Map();
  setTimeout = (callback, milliseconds) => {
    const id = ++this.nextId;
    this.timers.set(id, { callback, at: this.time + milliseconds });
    return id;
  };
  clearTimeout = id => { this.timers.delete(id); };
  async advance(milliseconds) {
    const until = this.time + milliseconds;
    while (true) {
      const next = [...this.timers].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      this.time = next[1].at;
      this.timers.delete(next[0]);
      next[1].callback();
      await flush();
    }
    this.time = until;
  }
}
const admission = (requestId = "request-A", extra = {}) => ({ requestId, architectId: "architect-1", promptSent: true, error: null, disposition: "new", ...extra });
const reviewed = { folder: "fixture", contextId: "context-1", requestedAt: 1, resolvedVersions: ["1.0.0"], declaredVersions: [], newestVersion: "2.0.0", newestStatus: "available" };
const state = (packageId = "A", status = "pending", extra = {}) => ({ requestId: "request-A", packageId, status, report: null, error: null, reviewed, ...extra });

test("ranking uses numeric stars, numeric contributors, then stable ids; unknown is not zero", () => {
  const nodes = ["project", "A", "B", "C", "D", "E", "F"].map(id => node(id));
  const metrics = new Map([
    ["A", info("A", 2, 5)], ["B", info("B", 2, 1)], ["C", info("C", 0, 3)],
    ["D", info("D", 10)], ["E", info("E", null, 0)], ["F", info("F", 2)],
  ]);
  assert.deepEqual(rankPackages(nodes, metrics).map(node => node.id), ["C", "B", "A", "F", "D", "E"]);
  assert.deepEqual(nodes.map(node => node.id), ["project", "A", "B", "C", "D", "E", "F"]);
  metrics.set("Z", info("Z", 2, 1));
  assert.deepEqual(rankPackages([node("Z"), node("B")], metrics).map(node => node.id), ["B", "Z"]);
  metrics.get("E").repository.stars = metric(0, "rateLimited");
  assert.equal(rankPackages(nodes, metrics).at(-1).id, "E");
  assert.equal(rankingIncomplete(nodes, metrics), true);
  assert.equal(rankingIncomplete([node("project"), node("C")], metrics), false);
});

test("formatCount preserves measured zero and distinguishes loading and remote states", () => {
  assert.equal(formatCount(metric(0)), "0");
  assert.equal(formatCount(metric(12345)), "12,345");
  assert.equal(formatCount(undefined), "Loading…");
  assert.equal(formatCount(metric(null)), "Unavailable");
  assert.equal(formatCount(metric(null, "rateLimited")), "Rate limited");
  assert.equal(formatCount(metric(null, "error")), "Unknown");
  assert.equal(formatCount(metric(null, "available")), "Unknown");
  assert.equal(formatCount(metric(-1)), "Unknown");
});

test("global ranking precedes 50-row pagination, including large collections", () => {
  const nodes = Array.from({ length: 3000 }, (_, index) => node(`npm:${index}`));
  const metrics = new Map(nodes.map((node, index) => [node.id, info(node.id, 3000 - index, 1)]));
  const first = packagePage(nodes, metrics, 0), second = packagePage(nodes, metrics, 1), last = packagePage(nodes, metrics, 59);
  assert.equal(first.length, 50);
  assert.equal(second.length, 50);
  assert.equal(last.length, 50);
  assert.equal(first[0].id, "npm:2999");
  assert.equal(first.at(-1).id, "npm:2950");
  assert.equal(second[0].id, "npm:2949");
  assert.equal(last.at(-1).id, "npm:0");
  assert.equal(nodes[0].id, "npm:0");
});

test("List ranking and Graph share every mode, basic filter and inclusive depth cap", () => {
  const graph = {
    folder: "fixture", key: "fixture", notes: [], truncated: false,
    nodes: [node("project"), node("A", { direct: true }), node("B", { direct: true }), node("T"), node("U")],
    edges: [["project", "A"], ["project", "B"], ["A", "T"], ["B", "U"]],
  };
  const classification = { items: { B: { basic: true, reason: "fixture" } }, updatedAt: 1 };
  for (const mode of ["direct", "transitive"]) for (const showAll of [true, false]) for (const layers of [null, 1, 2]) {
    const view = visibleGraph(graph, classification, mode, showAll, layers);
    assert.deepEqual(rankPackages(view.nodes, new Map()).map(node => node.id).sort(), view.nodes.filter(node => node.id !== "project").map(node => node.id).sort());
  }
  assert.deepEqual(rankPackages(visibleGraph(graph, classification, "transitive", false, 1).nodes, new Map()).map(node => node.id), ["A"]);
});

test("numeric majors require the full confirmed format and safe major integers", () => {
  for (const [version, major] of [["1.2", 1], [" v2.3.4-beta.1 ", 2], ["3.4.5+build.7", 3], ["0.1.0", 0], ["123.0", 123]]) assert.equal(parseNumericMajor(version), major, version);
  for (const version of ["1", "^1.2.3", "1.02.3", "01.2.3", "1.2.3.4", "release-1.2", "V1.2", "1.2 trailing", "1.2-", "1.2.3-beta+build", "9007199254740992.0.0"]) assert.equal(parseNumericMajor(version), null, version);
});

test("red lag uses all resolved copies, never declarations or the compatibility guess", () => {
  const newest = info("A");
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["3.9.0"] }), newest), 1);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["2.8.0", "3.0.0"] }), newest), 2);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["unknown", "1.0"] }), newest), 3);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["unknown", "3.0"] }), newest), null);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["5.0.0"] }), newest), 0);
  assert.equal(packageMajorLag(node("A", { version: "1.0.0", declaredVersions: ["1.0.0"] }), newest), null);
  assert.equal(packageMajorLag(node("project", { resolvedVersions: ["1.0"] }), newest), null);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["1.0"] }), info("A", 0, 0, { newestStatus: "error" })), null);
  assert.equal(packageMajorLag(node("A", { resolvedVersions: ["1.0"] }), info("A", 0, 0, { newestVersion: "release-next" })), null);
  assert.equal(nodeColor(node("A", { resolvedVersions: ["1.0"] }), true, packageMajorLag(node("A", { resolvedVersions: ["1.0"] }), newest)), "#f85149");
});

test("version labels separate evidence and retain registry release qualifiers", () => {
  assert.equal(projectVersionText(node("A", { declaredVersions: ["^1.0.0"] })), "Project version: not resolved · Declared: ^1.0.0");
  assert.equal(projectVersionText(node("A")), "Project version: not resolved · Declared: Unknown");
  assert.equal(projectVersionText(node("A", { resolvedVersions: ["1.0.0", "2.0.1"] })), "Project uses: 1.0.0, 2.0.1 (multiple resolved versions)");
  assert.match(projectVersionText(node("A", { ecosystem: "cargo", resolvedVersions: ["1.0.0"] })), /lockfile versions/);
  assert.equal(newestText(info("A", 0, 0, { newestVersion: "4.0.0-rc.1" })), "4.0.0-rc.1");
  assert.equal(newestText(undefined, "offline"), "Unknown");
});

test("public links permit HTTPS without credentials, and reports group null majors explicitly", () => {
  assert.equal(safePublicUrl("https://example.com/advisory"), "https://example.com/advisory");
  for (const value of ["javascript:alert(1)", "data:text/html,test", "file:///tmp/test", "http://example.com", "https://user:secret@example.com", "not a URL"]) assert.equal(safePublicUrl(value), null);
  const findings = [{ title: "one", majorVersion: "2" }, { title: "unknown", majorVersion: null }, { title: "two", majorVersion: "2" }];
  assert.deepEqual(groupFindings(findings).map(([major, items]) => [major, items.map(item => item.title)]), [["2", ["one", "two"]], ["Unspecified", ["unknown"]]]);
});

test("registry-only plans use skip, cap concurrency at four and deduplicate attempts", async () => {
  const calls = [], pending = [];
  const queue = new PackageInsightsQueue((id, mode, refresh) => { calls.push([id, mode, refresh]); const value = deferred(); pending.push(value); return value.promise; }, () => {});
  const plan = { demand: [], registry: ["project", "A", "B", "C", "D", "E", "A"], bulk: [] };
  queue.prioritize(plan);
  assert.deepEqual(calls, ["A", "B", "C", "D"].map(id => [id, "skip", false]));
  pending[0].resolve(info("A")); await flush();
  assert.equal(calls.length, 5);
  for (let i = 1; i < pending.length; i++) pending[i].resolve(info(calls[i][0]));
  await flush(); queue.prioritize(plan); await flush();
  assert.equal(calls.length, 5);
});

test("demand goes first, strongest mode wins earliest position and only one GitHub call runs", async () => {
  const calls = [], pending = [];
  const queue = new PackageInsightsQueue((id, mode) => { calls.push([id, mode]); const value = deferred(); pending.push(value); return value.promise; }, () => {});
  queue.prioritize({ demand: ["D", "D2"], registry: ["A", "B", "C"], bulk: ["A", "Z"] });
  assert.deepEqual(calls, [["D", "demand"], ["B", "skip"], ["C", "skip"]]);
  pending[0].resolve(info("D")); await flush();
  assert.deepEqual(calls[3], ["D2", "demand"]);
  pending[3].resolve(info("D2")); await flush();
  assert.deepEqual(calls[4], ["A", "bulk"]);
  queue.stop();
  for (const value of pending) value.resolve(info("ignored"));
  await flush();
});

test("generation reset discards queued work and ignores late calls; stop blocks work", async () => {
  const calls = [], pending = [];
  const queue = new PackageInsightsQueue((id, mode) => { calls.push(id); const value = deferred(); pending.push(value); return value.promise; }, () => {});
  queue.prioritize({ demand: ["old", "queued"], registry: [], bulk: [] });
  queue.reset(); queue.prioritize({ demand: ["new"], registry: [], bulk: [] });
  assert.deepEqual(calls, ["old"]);
  pending[0].resolve(info("old")); await flush();
  assert.deepEqual(calls, ["old", "new"]);
  assert.equal(queue.insights.has("old"), false);
  queue.stop(); pending[1].resolve(info("new")); await flush();
  assert.equal(queue.insights.size, 0);
});

test("attempted keys prevent errors repeating and forget re-enables GitHub requests", async () => {
  const calls = [];
  const queue = new PackageInsightsQueue(async (id, mode) => { calls.push(mode); throw new Error("offline"); }, () => {});
  const plan = { demand: ["A"], registry: [], bulk: [] };
  queue.prioritize(plan); await flush(); queue.prioritize(plan); await flush();
  assert.deepEqual(calls, ["demand"]); assert.equal(queue.errors.get("A"), "offline");
  queue.forget(["A"]); queue.prioritize(plan); await flush();
  assert.deepEqual(calls, ["demand", "demand"]);
});

test("registry data may upgrade to GitHub once and satisfied stats never repeat", async () => {
  const calls = [];
  const queue = new PackageInsightsQueue(async (id, mode) => { calls.push(mode); return info(id, 0, 0, { githubPending: mode === "skip" }); }, () => {});
  queue.prioritize({ demand: [], registry: ["A"], bulk: [] }); await flush();
  queue.prioritize({ demand: ["A"], registry: ["A"], bulk: [] }); await flush();
  queue.forget(["A"]); queue.prioritize({ demand: [], registry: ["A"], bulk: ["A"] }); await flush();
  assert.deepEqual(calls, ["skip", "demand"]);
});

test("refresh is immediate, deduplicated and replaces data while rejection preserves it", async () => {
  let pending = deferred(), calls = [];
  const queue = new PackageInsightsQueue((...args) => { calls.push(args); return pending.promise; }, () => {});
  const old = info("A"); queue.insights.set("A", old);
  const refreshing = queue.refresh("A"); await queue.refresh("A");
  assert.equal(queue.refreshing.has("A"), true);
  assert.deepEqual(calls, [["A", "demand", true]]);
  const fresh = info("A", 100); pending.resolve(fresh); await refreshing;
  assert.equal(queue.insights.get("A"), fresh); assert.equal(queue.refreshing.has("A"), false);
  pending = deferred();
  const failed = queue.refresh("A"); pending.reject(new Error("offline")); await failed;
  assert.equal(queue.insights.get("A"), fresh); assert.equal(queue.errors.get("A"), "offline");
});

test("reviews poll once per second, complete with report, and a later click admits a fresh request", async () => {
  const clock = new Clock(); let starts = 0, reads = 0;
  const report = { summary: "Synthetic sourced report", findings: [], limitations: ["fixture"], completedAt: 1000 };
  const session = new DependencyReviewSession({
    start: async () => { starts++; return admission(); },
    read: async () => { reads++; return state("A", reads === 1 ? "pending" : "complete", reads === 1 ? {} : { report }); },
  }, () => {}, clock);
  await session.open("fixture", "A"); await flush();
  assert.equal(reads, 1);
  await clock.advance(999); assert.equal(reads, 1);
  await clock.advance(1); assert.equal(reads, 2);
  assert.equal(session.snapshot.state.report, report);
  assert.equal(session.snapshot.pending, false);
  assert.equal(clock.timers.size, 0);
  await clock.advance(10000); assert.equal(reads, 2);
  await session.open("fixture", "A"); await flush(); assert.equal(starts, 2);
  session.close();
});

test("each open asks the backend again; only unsettled admission calls are shared", async () => {
  const clock = new Clock(), accepted = deferred(); let starts = 0, reads = 0;
  const session = new DependencyReviewSession({
    start: () => { starts++; return accepted.promise; }, read: async () => { reads++; return state(); },
  }, () => {}, clock);
  const first = session.open("fixture", "A"); session.close();
  const second = session.open("fixture", "A");
  accepted.resolve(admission("request-A", { promptSent: null, error: "Delivery uncertain" }));
  const [one, two] = await Promise.all([first, second]); await flush();
  assert.equal(starts, 1); assert.equal(reads, 1);
  assert.equal(one, null);
  assert.equal(two.promptSent, null); assert.equal(two.disposition, "new");
  assert.equal(session.snapshot.admission.promptSent, null);
  assert.equal(session.snapshot.pending, true);
  session.close(); await clock.advance(5000); assert.equal(reads, 1);
  const third = await session.open("fixture", "A"); await flush();
  assert.equal(starts, 2); assert.equal(reads, 2); assert.equal(third.requestId, "request-A");
  assert.equal(session.snapshot.requestId, "request-A");
  session.close();
});

test("reopening reads once before deciding: a result that completed while closed reads as complete", async () => {
  const clock = new Clock(); let reads = 0;
  const report = { summary: "Saved", findings: [], limitations: [], completedAt: 5 };
  const session = new DependencyReviewSession({
    start: async (_folder, _id, rerun) => admission("request-A", { disposition: rerun ? "new" : "pending" }),
    read: async () => { reads++; return state("A", "complete", { report }); },
  }, () => {}, clock);
  const opened = await session.open("fixture", "A");
  assert.equal(reads, 1); assert.equal(opened.disposition, "complete");
  assert.equal(session.snapshot.admission.disposition, "complete");
  assert.equal(session.snapshot.state.report, report); assert.equal(session.snapshot.pending, false);
  assert.equal(session.snapshot.state.reviewed.resolvedVersions[0], "1.0.0");
  assert.equal(clock.timers.size, 0);
  await clock.advance(60000); assert.equal(reads, 1);
  const reused = await session.open("fixture", "A"); assert.equal(reused.disposition, "complete"); assert.equal(reads, 2);
});

test("rerun is forwarded explicitly, pending disposition survives and error results stop polling", async () => {
  const clock = new Clock(), reruns = []; let phase = "pending";
  const session = new DependencyReviewSession({
    start: async (_folder, _id, rerun) => { reruns.push(rerun); return admission("request-A", { disposition: "pending" }); },
    read: async () => state("A", phase, phase === "error" ? { error: "producerEnded" } : {}),
  }, () => {}, clock);
  assert.equal((await session.open("fixture", "A")).disposition, "pending");
  phase = "error";
  const failed = await session.open("fixture", "A", true);
  assert.deepEqual(reruns, [false, true]);
  assert.equal(failed.disposition, "error"); assert.equal(session.snapshot.error, "producerEnded");
  assert.equal(session.snapshot.pending, false); assert.equal(clock.timers.size, 0);
});

test("a closed popup frees the waiting open, ignores late admission and report completions", async () => {
  const clock = new Clock(), late = deferred(), report = deferred();
  const session = new DependencyReviewSession({
    start: async (_folder, id) => id === "A" ? late.promise : admission("request-B"),
    read: () => report.promise,
  }, () => {}, clock);
  const old = session.open("fixture", "A");
  const next = session.open("fixture", "B"); await flush();
  late.resolve(admission()); assert.equal(await old, null); await flush();
  assert.equal(session.snapshot.packageId, "B");
  session.close(); assert.equal(await next, null);
  report.resolve(state("B", "complete", { requestId: "request-B" })); await flush();
  assert.equal(session.snapshot.packageId, null);
  assert.equal(session.snapshot.state, null);
  assert.equal(clock.timers.size, 0);
});



test("folder generation rejects stale admissions and clears pending dedupe for the new folder", async () => {
  const clock = new Clock(), old = deferred(); let starts = 0;
  const session = new DependencyReviewSession({ start: () => { starts++; return starts === 1 ? old.promise : Promise.resolve(admission("new")); }, read: async () => state("A", "pending", { requestId: "new" }) }, () => {}, clock);
  const first = session.open("old-folder", "A"); session.reset();
  await session.open("new-folder", "A"); await flush();
  old.resolve(admission("old")); await first; await flush();
  assert.equal(starts, 2);
  assert.equal(session.snapshot.admission.requestId, "new");
  session.close();
});

test("review deadline caps stalled reads at ten minutes and ignores their late results", async () => {
  const clock = new Clock(), lateRead = deferred(); let reads = 0;
  const session = new DependencyReviewSession({ start: async () => admission(), read: () => { reads++; return lateRead.promise; } }, () => {}, clock);
  const opened = session.open("fixture", "A"); await flush();
  await clock.advance(599999); assert.equal(session.snapshot.timedOut, false); assert.equal(reads, 1);
  await clock.advance(1); assert.equal(session.snapshot.timedOut, true); assert.equal(session.snapshot.pending, false);
  assert.equal(await opened, null);
  lateRead.resolve(state("A", "complete")); await flush();
  assert.equal(session.snapshot.state, null);
  assert.equal(clock.timers.size, 0);
});

test("root does not admit a review; missing Architect, known refusal and mismatched reports stay explicit", async () => {
  const clock = new Clock(); let starts = 0;
  const session = new DependencyReviewSession({ start: async () => { starts++; throw new Error("Create The Architect first using its toolbar button."); }, read: async () => state() }, () => {}, clock);
  await session.open("fixture", "project"); assert.equal(starts, 0);
  await session.open("fixture", "A"); assert.match(session.snapshot.error, /Create The Architect/); assert.equal(session.snapshot.pending, false);
  assert.equal(clock.timers.size, 0);
  const refused = new DependencyReviewSession({ start: async () => admission("refused", { promptSent: false, error: "bridge unavailable" }), read: async () => { assert.fail("refused review must not poll"); } }, () => {}, clock);
  await refused.open("fixture", "A"); assert.equal(refused.snapshot.error, "bridge unavailable"); assert.equal(refused.snapshot.pending, false);
  const mismatch = new DependencyReviewSession({ start: async () => admission(), read: async () => state("B") }, () => {}, clock);
  await mismatch.open("fixture", "A"); await flush();
  assert.match(mismatch.snapshot.error, /did not match/); assert.equal(mismatch.snapshot.pending, false);
  assert.equal(clock.timers.size, 0);
});

test("the generic job session serves usage, project and update-plan flows with their own subjects", async () => {
  const clock = new Clock(), starts = []; let reads = 0;
  const session = new DependencyJobSession({
    start: async (folder, subject, rerun) => { starts.push([folder, subject, rerun]); return admission("job-1", { disposition: "pending" }); },
    read: async id => { reads++; return { requestId: id, folder: "fixture", contextId: null, status: reads === 1 ? "pending" : "complete", coverage: [], summary: reads === 1 ? null : "done", limitations: [], completedAt: null, error: null }; },
  }, () => {}, clock);
  const opened = await session.open("fixture", null, false);
  assert.deepEqual(starts, [["fixture", null, false]]);
  assert.equal(opened.disposition, "pending"); assert.equal(session.snapshot.pending, true);
  await clock.advance(1000);
  assert.equal(session.snapshot.state.summary, "done"); assert.equal(session.snapshot.pending, false);
  assert.equal(session.snapshot.requestId, "job-1"); assert.equal(clock.timers.size, 0);
  await clock.advance(30000); assert.equal(reads, 2);
  // A state naming another request is refused instead of displayed.
  const mismatch = new DependencyJobSession({ start: async () => admission("job-2"), read: async () => ({ requestId: "other", status: "complete", error: null }) }, () => {}, clock);
  assert.equal((await mismatch.open("fixture", "review-1", true)).requestId, "job-2");
  assert.match(mismatch.snapshot.error, /did not match/); assert.equal(mismatch.snapshot.state, null);
  mismatch.reset(); assert.equal(mismatch.snapshot.requestId, null);
  assert.equal(clock.timers.size, 0);
});

test("GitHub budget text names the credential, the remaining budget and the cause of anonymous access", () => {
  const base = { githubAuthenticated: true, githubLimit: 5000, githubRemaining: 4990, githubResetAt: 1000, githubCooldownUntil: null, githubSource: "gh", githubProblem: null };
  const at = value => `t${value}`;
  assert.equal(githubStatusText(base, at), "GitHub API: 4990/5000 left · resets t1000 · token (gh)");
  assert.equal(githubStatusText({ ...base, githubRemaining: null, githubResetAt: null }, at), "GitHub API: 5000/hour · token (gh)");
  assert.equal(githubStatusText({ ...base, githubLimit: null, githubRemaining: null, githubSource: null }, at), "GitHub API: token");
  const anonymous = { ...base, githubAuthenticated: false, githubLimit: 60, githubRemaining: 48, githubSource: null, githubProblem: "GitHub CLI (gh) was not found." };
  assert.equal(githubStatusText(anonymous, at), "GitHub API: 48/60 left · resets t1000 · anonymous");
  assert.equal(githubStatusText({ ...anonymous, githubRemaining: null, githubResetAt: null }, at), "GitHub API: 60/hour · anonymous");
  assert.equal(githubStatusText({ ...anonymous, githubLimit: null, githubRemaining: null, githubResetAt: null, githubProblem: null }, at), "GitHub API: anonymous");
  assert.equal(githubProblemText(anonymous), "GitHub CLI (gh) was not found.");
  assert.equal(githubProblemText(base), null);
});
