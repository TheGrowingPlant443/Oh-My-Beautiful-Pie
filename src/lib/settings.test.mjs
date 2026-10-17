import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseAppSettings, resolveStartupTarget, dashboardInstances, startsFresh } from './settings.ts';

const rows = [
  { id: 'M', kind: 'manager', status: 'stopped' },
  { id: 'A', kind: 'instance', status: 'stopped' },
  { id: 'B', kind: 'instance', status: 'running' },
  { id: 'C', kind: 'instance', status: 'stopped' },
];

test('initial and migrated preferences open Manager without exposing restored history', () => {
  const settings = parseAppSettings(null, 'off');
  assert.equal(settings.autoStart, true);
  assert.equal(settings.showPreviousProjects, false);
  assert.equal(settings.showManagerTile, false);
  assert.deepEqual(resolveStartupTarget(settings, rows), { instanceId: 'M', missingProject: false });
  assert.deepEqual(dashboardInstances(rows, settings, new Set(['A', 'B']), new Set(['B'])).map(r => r.id), ['B', 'C']);
});

test('malformed records and independently invalid fields cannot create invalid targets', () => {
  for (const raw of ['{', 'null', '[]', '42', '{"version":2,"startupTarget":"project"}']) {
    const settings = parseAppSettings(raw, null);
    assert.deepEqual(resolveStartupTarget(settings, rows), { instanceId: 'M', missingProject: false });
    assert.equal(settings.showPreviousProjects, false);
  }
  const settings = parseAppSettings(JSON.stringify({ version: 1, startupTarget: 'invalid', startupProjectId: 7,
    latestProjectId: ' ', previewSize: -1, autoStart: 'false', showManagerTile: false, showPreviousProjects: 'true' }), null);
  assert.equal(settings.previewSize, 640);
  assert.equal(settings.autoStart, true);
  assert.equal(settings.showManagerTile, false);
  assert.equal(settings.latestProjectId, null);
  assert.equal(settings.startupProjectId, null);
  assert.deepEqual(resolveStartupTarget(settings, rows), { instanceId: 'M', missingProject: false });
  assert.deepEqual(dashboardInstances(rows, settings, new Set(['A', 'B']), new Set(['A'])).map(r => r.id), ['A', 'C']);
});

test('chosen and latest targets use instance identity and fall back only to Manager', () => {
  const base = parseAppSettings(null, null);
  assert.deepEqual(resolveStartupTarget({ ...base, startupTarget: 'project', startupProjectId: 'A' }, rows), { instanceId: 'A', missingProject: false });
  assert.deepEqual(resolveStartupTarget({ ...base, startupTarget: 'latest', latestProjectId: 'B' }, rows), { instanceId: 'B', missingProject: false });
  for (const id of [null, 'deleted', 'M']) {
    const settings = { ...base, startupTarget: 'project', startupProjectId: id };
    assert.deepEqual(resolveStartupTarget(settings, rows), { instanceId: 'M', missingProject: true });
    assert.deepEqual(resolveStartupTarget(settings, rows.slice(1)), { instanceId: null, missingProject: true });
    assert.deepEqual(resolveStartupTarget({ ...base, startupTarget: 'latest', latestProjectId: id }, rows), { instanceId: 'M', missingProject: true });
  }
  assert.deepEqual(resolveStartupTarget(base, rows.slice(1)), { instanceId: null, missingProject: false });
});

test('dashboard startup is intentional with or without Manager and preserves saved choices', () => {
  for (const autoStart of [true, false]) {
    const saved = { version: 1, startupTarget: 'dashboard', startupProjectId: 'deleted-project',
      latestProjectId: 'deleted-latest', autoStart, showPreviousProjects: true, showManagerTile: false, previewSize: 1000, browserHeadless: true, browserEmbedded: false, terminalAutoOpen: true, terminalCreateUser: false, alwaysNewSession: true,
      packageLookupGate: true, manifestFilter: true, filterLanguages: ["maven", "npm"], aiFilter: true, filterRoles: ["backend"] };
    const settings = parseAppSettings(JSON.stringify(saved), null);
    assert.deepEqual(settings, saved);
    assert.deepEqual(parseAppSettings(JSON.stringify(settings), null), settings);
    for (const list of [rows, rows.slice(1), []]) {
      assert.deepEqual(resolveStartupTarget(settings, list), { instanceId: null, missingProject: false });
    }
    assert.deepEqual(dashboardInstances(rows, settings, new Set(['A', 'B']), new Set()).map(r => r.id), ['A', 'B', 'C']);
  }
});

test('history visibility preserves order, opened stopped projects and new workers', () => {
  const settings = parseAppSettings(null, null);
  const restored = new Set(['A', 'B']);
  const opened = new Set(['B']);
  assert.deepEqual(dashboardInstances(rows, settings, restored, opened).map(r => r.id), ['M', 'B', 'C']);
  assert.deepEqual(dashboardInstances(rows, { ...settings, showManagerTile: false }, restored, opened).map(r => r.id), ['B', 'C']);
  assert.deepEqual(dashboardInstances(rows, { ...settings, showPreviousProjects: true }, restored, opened).map(r => r.id), ['M', 'A', 'B', 'C']);
  assert.deepEqual(dashboardInstances(rows, settings, restored, new Set(['A'])).map(r => r.id), ['M', 'A', 'C']);
});

test('browser preferences validate independently and preserve startup choices', () => {
  const defaults = parseAppSettings(null, null);
  assert.equal(defaults.browserHeadless, false);
  assert.equal(defaults.browserEmbedded, true);
  for (const [headless, embedded] of [[true, false], [false, true], [true, true], [false, false]]) {
    const saved = { ...defaults, startupTarget: 'latest', latestProjectId: 'B', browserHeadless: headless, browserEmbedded: embedded };
    assert.deepEqual(parseAppSettings(JSON.stringify(saved), null), saved);
  }
  const invalidHeadless = parseAppSettings(JSON.stringify({ version: 1, browserHeadless: 'true', browserEmbedded: false }), null);
  assert.equal(invalidHeadless.browserHeadless, false);
  assert.equal(invalidHeadless.browserEmbedded, false);
  const invalidEmbedded = parseAppSettings(JSON.stringify({ version: 1, browserHeadless: true, browserEmbedded: null }), null);
  assert.equal(invalidEmbedded.browserHeadless, true);
  assert.equal(invalidEmbedded.browserEmbedded, true);
});

test('terminal preferences validate independently and default to a quiet pane with a user shell', () => {
  const defaults = parseAppSettings(null, null);
  assert.equal(defaults.terminalAutoOpen, false);
  assert.equal(defaults.terminalCreateUser, true);
  const mixed = parseAppSettings(JSON.stringify({ version: 1, terminalAutoOpen: true, terminalCreateUser: 'no' }), null);
  assert.equal(mixed.terminalAutoOpen, true);
  assert.equal(mixed.terminalCreateUser, true);
  const off = parseAppSettings(JSON.stringify({ version: 1, terminalAutoOpen: 1, terminalCreateUser: false }), null);
  assert.equal(off.terminalAutoOpen, false);
  assert.equal(off.terminalCreateUser, false);
});

test('the new-session preference defaults to resuming and ignores non-boolean values', () => {
  assert.equal(parseAppSettings(null, null).alwaysNewSession, false);
  assert.equal(parseAppSettings(JSON.stringify({ version: 1, alwaysNewSession: 'yes' }), null).alwaysNewSession, false);
  assert.equal(parseAppSettings(JSON.stringify({ version: 1, alwaysNewSession: true }), null).alwaysNewSession, true);
});

test('a new session is started only for ordinary agents without an explicitly chosen session', () => {
  const on = parseAppSettings(JSON.stringify({ version: 1, alwaysNewSession: true }), null);
  const off = parseAppSettings(null, null);
  const ordinary = { planTarget: null, helper: null };
  assert.equal(startsFresh(off, ordinary, false), false);
  assert.equal(startsFresh(on, ordinary, false), true);
  assert.equal(startsFresh(on, ordinary, true), false);
  assert.equal(startsFresh(on, { planTarget: null, helper: { target: 'a', purpose: 'git', repo: 'x' } }, false), false);
  assert.equal(startsFresh(on, { planTarget: 'x', helper: null }, false), false);
  assert.equal(startsFresh(on, undefined, false), false);
});

test("dependency lookup and filter settings default off and normalise untrusted arrays", () => {
  const defaults = parseAppSettings(null, null);
  for (const key of ["packageLookupGate", "manifestFilter", "aiFilter"]) assert.equal(defaults[key], false);
  assert.deepEqual(defaults.filterLanguages, []);
  assert.deepEqual(defaults.filterRoles, []);
  const parsed = parseAppSettings(JSON.stringify({ version: 1, packageLookupGate: "yes", manifestFilter: 1, aiFilter: null, filterLanguages: ["java", "npm", "npm", 7], filterRoles: ["frontend", "frontend", "library", 7] }), null);
  assert.equal(parsed.packageLookupGate, false);
  assert.equal(parsed.manifestFilter, false);
  assert.equal(parsed.aiFilter, false);
  assert.deepEqual(parsed.filterLanguages, ["npm"]);
  assert.deepEqual(parsed.filterRoles, ["frontend"]);
  const invalid = parseAppSettings(JSON.stringify({ version: 1, filterLanguages: "npm", filterRoles: {} }), null);
  assert.deepEqual(invalid.filterLanguages, []);
  assert.deepEqual(invalid.filterRoles, []);
});
