import test from 'node:test';
import assert from 'node:assert/strict';
import { nativeAnswer, controlAllowed, planPayload, confirmedPlanHandoff, managerParameters, managerPayload, registerManagerTool } from './omp-manager.mjs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

test('select answers must be actual native labels, not truthy values', () => {
  const request = { method: 'select', options: ['Approve', 'Deny'] };
  assert.equal(nativeAnswer(request, 'Deny'), 'Deny');
  assert.equal(nativeAnswer(request, 'Approve'), 'Approve');
  assert.throws(() => nativeAnswer(request, true));
  assert.throws(() => nativeAnswer(request, 'approve'));
  assert.equal(nativeAnswer(request, { cancelled: true }), undefined);
});
test('remote approvals grant only this call while confirmations remain boolean', () => {
  const approval = { method: 'approve' };
  assert.equal(nativeAnswer(approval, true), 'once');
  assert.equal(nativeAnswer(approval, false), 'deny');
  assert.equal(nativeAnswer(approval, { cancelled: true }), 'deny');
  for (const response of ['Approve', 'once', 'session', { grant: 'target', always: true }]) {
    assert.throws(() => nativeAnswer(approval, response), /boolean/);
  }
  const confirmation = { method: 'confirm' };
  assert.equal(nativeAnswer(confirmation, true), true);
  assert.equal(nativeAnswer(confirmation, false), false);
  assert.equal(nativeAnswer(confirmation, { cancelled: true }), false);
  assert.throws(() => nativeAnswer(confirmation, 'deny'), /boolean/);
});

test('cancelled native text and Ask answers stay undefined rather than becoming negative confirmations', () => {
  for (const method of ['input', 'editor', 'askDialog']) {
    assert.equal(nativeAnswer({ method }, { cancelled: true }), undefined);
  }
});
test('rich answers retain native question identity and enforce selection rules', () => {
  const request = { method: 'askDialog', questions: [{ id: 'design', question: 'Design?', options: [{label:'Simple'},{label:'Complex'}] }] };
  const answer = nativeAnswer(request, { kind:'submit', results:[{id:'design',question:'forged',options:['forged'],selectedOptions:['Simple'],note:'Keep it auditable'}] });
  assert.equal(answer.results[0].question, 'Design?');
  assert.deepEqual(answer.results[0].options, ['Simple','Complex']);
  assert.equal(answer.results[0].note, 'Keep it auditable');
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'different',selectedOptions:['Simple']}]}));
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'design',selectedOptions:['Simple','Complex']}]}));
  assert.throws(() => nativeAnswer(request, {kind:'submit',results:[{id:'design',selectedOptions:['Simple','Simple']}]}));
});
test('disabled options and opaque native overlays cannot be auto-approved', () => {
  assert.throws(() => nativeAnswer({method:'select',options:['Run','Keep'],disabledIndices:[0]}, 'Run'));
  assert.throws(() => nativeAnswer({method:'terminal'}, 'Approve and execute'), /native_terminal_decision/);
  assert.throws(() => nativeAnswer({method:'custom'}, true), /native_terminal_decision/);
  assert.throws(() => nativeAnswer({ method: 'custom' }, { cancelled: true }), /native_terminal_decision/);
  assert.throws(() => nativeAnswer({ method: 'terminal' }, { cancelled: true }), /native_terminal_decision/);
});
test('app-originated abort and prompt reach any session; manager-originated control still needs an owned instance', () => {
  assert.equal(controlAllowed({ source: 'human', action: 'abort', role: 'manager', owner: null }), true);
  assert.equal(controlAllowed({ source: 'human', action: 'prompt', role: 'instance', owner: null }), true);
  assert.equal(controlAllowed({ source: 'human', action: 'reply_ui', role: 'instance', owner: null }), true);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'instance', owner: null }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'manager', owner: 'M' }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'abort', role: 'instance', owner: 'M' }), true);
});
test('dependency_result is a human-origin, display-only action never reachable by a Manager or model', () => {
  assert.equal(controlAllowed({ source: 'human', action: 'dependency_result', role: 'instance', owner: null }), true);
  assert.equal(controlAllowed({ source: 'manager', action: 'dependency_result', role: 'instance', owner: 'M' }), false);
  assert.equal(controlAllowed({ source: 'manager', action: 'dependency_result', role: 'manager', owner: 'M' }), false);
});
test('project_visualization_prompt is a human-origin control, never available to a Manager, owner or missing source', () => {
  assert.equal(controlAllowed({ source: 'human', action: 'project_visualization_prompt', role: 'instance', owner: null }), true);
  for (const source of ['manager', undefined, 'model']) {
    assert.equal(controlAllowed({ source, action: 'project_visualization_prompt', role: 'instance', owner: 'M' }), false);
    assert.equal(controlAllowed({ source, action: 'project_visualization_prompt', role: 'manager', owner: 'M' }), false);
  }
});
test('plan payloads may be objects or JSON strings but never anything else', () => {
  assert.deepEqual(planPayload({ text: 'a' }), { text: 'a' });
  assert.deepEqual(planPayload('{"text": "# test\\n- [ ] one\\n"}'), { text: '# test\n- [ ] one\n' });
  assert.deepEqual(planPayload(undefined), {});
  assert.deepEqual(planPayload(null), {});
  for (const bad of ['not json', '"a string"', '[1]', 5, ['x']]) assert.throws(() => planPayload(bad), /payload must be a JSON object/);
});

test('a pending or declined confirmation cannot start or hand off; every retry asks again', async () => {
  let answer;
  let confirmations = 0;
  let handoffs = 0;
  const request = async action => {
    if (action === 'plan_handoff_info') return { name: 'Worker', revision: 'r1', needsStart: true };
    handoffs++;
    return { accepted: true };
  };
  const ctx = { hasUI: true, ui: { confirm: () => {
    confirmations++;
    return new Promise(resolve => { answer = resolve; });
  } } };
  const pending = confirmedPlanHandoff(request, ctx, { summary: 'shorter delays', confirmed: true });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(handoffs, 0);
  answer(false);
  assert.deepEqual(await pending, { accepted: false, cancelled: true });
  assert.equal(handoffs, 0);
  const retry = confirmedPlanHandoff(request, ctx, { summary: 'shorter delays' });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(confirmations, 2);
  answer(true);
  assert.equal((await retry).accepted, true);
  assert.equal(handoffs, 1);
});

test('headless and aborted handoffs cannot perform target mutations', async () => {
  let mutations = 0;
  const request = async action => {
    if (action === 'plan_handoff_info') return { name: 'Worker', revision: 'r1', needsStart: false };
    mutations++;
  };
  await assert.rejects(confirmedPlanHandoff(request, { hasUI: false }, { summary: 'change' }), /human confirmation/);
  const controller = new AbortController();
  const ctx = { hasUI: true, ui: { confirm: async () => { controller.abort(new Error('cancelled')); return true; } } };
  await assert.rejects(confirmedPlanHandoff(request, ctx, { summary: 'change' }, controller.signal), /cancelled/);
  assert.equal(mutations, 0);
});

const sdk = process.env.OMP_SDK_DIR;
const sdkTest = sdk ? {} : { skip: 'Set OMP_SDK_DIR to exercise installed pi.zod' };
async function nativeZod() {
  return import(pathToFileURL(path.join(sdk, '..', 'omptype', 'dist', 'js', 'zod.js')).href);
}

test('flat Manager schema covers every canonical action and preserves explicit values', sdkTest, async () => {
  const schema = managerParameters(await nativeZod());
  const calls = [
    {action:'list'},
    {action:'create',name:'API',projectPath:'C:/api',useGlobalConfig:false,start:false,prompt:''},
    ...['start','stop','restart','remove','interrupt','screen','plan','subagents'].map(action => ({action,instanceId:'owned-b'})),
    {action:'send',instanceId:'owned-b',message:'',delivery:'follow_up'},
    {action:'transcript',instanceId:'owned-b',subagentId:'child'},
    {action:'keys',instanceId:'owned-b',generation:0,data:'',sessionId:null},
    {action:'respond',instanceId:'owned-b',requestId:'decision',generation:0,response:false},
    {action:'subscribe',instanceId:'owned-b',enabled:false},
    {action:'request_human',instanceId:'owned-b',reason:'Need context',requestId:'decision'},
  ];
  for (const call of calls) {
    const payload = managerPayload(schema.parse(call));
    if (call.instanceId) assert.equal(payload.instanceId, 'owned-b');
    if (call.action !== 'list' && call.action !== 'create') {
      const {instanceId, ...missing} = call;
      assert.throws(() => managerPayload(schema.parse(missing)), new RegExp(`${call.action} requires instanceId`));
    }
  }
  assert.deepEqual(managerPayload(schema.parse(calls[1])), {
    config:{name:'API',projectPath:'C:/api',useGlobalConfig:false},start:false,prompt:'',
  });
  assert.deepEqual(managerPayload(schema.parse({action:'subscribe',instanceId:'b',enabled:false})), {instanceId:'b',enabled:false});
  for (const call of [
    {action:'screen',payload:{instanceId:'b'}}, {action:'create',config:{name:'x'}},
    {action:'screen',instanceId:'b',i:'not application owned'},
    {action:'keys',instanceId:'b',generation:-1,data:''},
    {action:'keys',instanceId:'b',generation:1.5,data:''},
    {action:'respond',instanceId:'b',requestId:'r',generation:0,response:{kind:'submit',results:[{id:'q',selectedOptions:[],question:'forged'}]}},
  ]) assert.throws(() => schema.parse(call));
  for (const call of [{action:'list',instanceId:'b'}, {action:'screen',instanceId:'b',message:'wrong action'},
    {action:'send',instanceId:'b'}, {action:'subscribe',instanceId:'b'}]) {
    assert.throws(() => managerPayload(schema.parse(call)));
  }
});

test('registered Manager tool uses installed SDK validator before any transport on invalid calls', sdkTest, async () => {
  const script = `
  import assert from 'node:assert/strict';
  import {registerManagerTool} from ${JSON.stringify(new URL('./omp-manager.mjs',import.meta.url).href)};
  import * as zod from ${JSON.stringify(pathToFileURL(path.join(sdk,'..','omptype','src','zod.ts')).href)};
  import {validateToolArguments} from ${JSON.stringify(pathToFileURL(path.join(sdk,'..','pi-ai','src','utils','validation.ts')).href)};
  import {normalizeTools} from ${JSON.stringify(pathToFileURL(path.join(sdk,'..','pi-agent-core','src','agent-loop.ts')).href)};
  let tool, transports = 0;
  registerManagerTool({zod,registerTool: value => {tool=value;}}, async () => { transports++; throw new Error('unexpected transport'); });
  const wire = normalizeTools([tool], {injectIntent:true})[0];
  assert.ok(wire.parameters.properties.i);
  assert.ok(wire.parameters.properties.instanceId);
  assert.equal(tool.parameters.safeParse({action:'screen',instanceId:'b',i:'Inspecting worker state'}).success,false);
  validateToolArguments(wire,{type:'toolCall',id:'wire',name:tool.name,arguments:{action:'screen',instanceId:'owned-b',i:'Inspecting worker state'}});
  const valid = validateToolArguments(tool, {type:'toolCall',id:'screen',name:tool.name,arguments:{action:'screen',instanceId:'owned-b'}});
  assert.equal(valid.instanceId, 'owned-b');
  const result = await tool.execute('missing', {action:'screen'}, undefined, undefined, {agent:{kind:'main'}});
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /screen requires instanceId; use/);
  for (const args of [{action:'screen',payload:'{"instanceId":"b"}'},
    {action:'respond',instanceId:'b',requestId:'r',generation:0,response:{kind:'submit',results:[{id:'q',selectedOptions:[],multi:true}]}}]) {
    const rejected = await tool.execute('bad', args, undefined, undefined, {agent:{kind:'main'}});
    assert.equal(rejected.isError,true);
  }
  // The SDK may repair unknown keys; it must not invent a target from legacy payload.
  const repaired = validateToolArguments(tool,{type:'toolCall',id:'legacy',name:tool.name,arguments:{action:'screen',payload:{instanceId:'owned-b'}}});
  assert.equal(repaired.instanceId,undefined);
  assert.equal((await tool.execute('legacy',repaired,undefined,undefined,{agent:{kind:'main'}})).isError,true);
  assert.equal(transports,0);
  `;
  const result = spawnSync(process.env.BUN_BIN || 'bun', ['--eval',script], {
    encoding:'utf8',env:{...process.env,PI_NO_INTENT:'0'},timeout:30000,
  });
  assert.equal(result.status,0,result.stderr || result.error?.message || result.stdout);
});
