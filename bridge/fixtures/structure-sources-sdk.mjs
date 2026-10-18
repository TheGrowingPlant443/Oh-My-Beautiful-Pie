import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { architectIdentity } from '../omp-architect.mjs';
import { createStructureSources } from '../omp-structure-sources.mjs';

// Real SDK session + real `local://` mapping (resolveLocalUrlToPath/resolveLocalRoot from the installed SDK); no provider, no model.
const root = process.env.STRUCTURE_SOURCES_TEST_ROOT;
const sdk = process.env.OMP_MANAGER_SDK_DIR;
assert.ok(root && sdk);
const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
dirs.setProfile('default');
for (const actual of [dirs.getAgentDir(), dirs.getConfigRootDir(), dirs.getSessionsDir()]) {
  assert.equal(path.relative(root, actual).startsWith('..'), false, `${actual} must stay below the scratch root`);
}
const { SessionManager } = await import(pathToFileURL(path.join(sdk, 'src/session/session-manager.ts')).href);
const protocol = await import(pathToFileURL(path.join(sdk, 'src/internal-urls/local-protocol.ts')).href);
const manager = SessionManager.create(path.join(root, 'project'));
const options = { getArtifactsDir: () => manager.getArtifactsDir(), getSessionId: () => manager.getSessionId() };
const localRoot = protocol.resolveLocalRoot(options);
await mkdir(localRoot, { recursive: true });
const body = '# Target layout\n\nsrc/\n  routes/\n';
await writeFile(protocol.resolveLocalUrlToPath('local://paste-1.md', options), body);

const env = { OMP_MANAGER_BRIDGE_TOKEN: 'token', OMP_MANAGER_BRIDGE_INSTANCE_ID: 'architect-1', OMP_MANAGER_BRIDGE_GENERATION: '1' };
const ctx = { mode: 'tui', agent: { kind: 'main' }, sessionManager: manager, localProtocolOptions: options };
// Default SDK loader (OMP_MANAGER_SDK_DIR) is exercised, not an injected resolver.
const sources = createStructureSources({ dir: path.join(root, 'architect'), identityFor: c => architectIdentity(c, env), currentContext: () => ctx });
await sources.capture({ type: 'input', text: 'layout is in local://paste-1.md', source: 'interactive' }, ctx);
const { sessionId, sources: rows } = await sources.control(ctx, 'structure_sources', {});
assert.equal(sessionId, manager.getSessionId());
assert.equal(rows.length, 1);
assert.equal(rows[0].error, null);
assert.equal(rows[0].kind, 'interactiveLocalPaste');
assert.equal(rows[0].sha256, createHash('sha256').update(body).digest('hex'));
const page = await sources.control(ctx, 'structure_source_read', { sourceId: rows[0].sourceId, offset: 0 });
assert.equal(page.text, body);
assert.equal(page.nextOffset, null);
// A missing paste is a visible unusable record from the same real mapping.
await sources.capture({ type: 'input', text: 'local://paste-2.md', source: 'interactive' }, ctx);
const after = (await sources.control(ctx, 'structure_sources', {})).sources;
assert.equal(after.length, 2);
assert.match(after.find(row => row.error).error, /does not exist/);
await manager.close();
console.log('Structure source local paste capture verified against the installed SDK local:// mapping');
