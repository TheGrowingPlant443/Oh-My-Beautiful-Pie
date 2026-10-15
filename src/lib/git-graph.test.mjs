import {test} from 'node:test';
import assert from 'node:assert/strict';
import {layoutGraph,parseRefs} from './git-graph.ts';

const commit=(hash,parents=[])=>({hash,parents,author:'Test',date:'2026-01-01',subject:hash,refs:[]});

test('linear history remains in one lane with ordered parent links',()=>{
 const result=layoutGraph([commit('c',['b']),commit('b',['a']),commit('a')]);
 assert.deepEqual([...result.positions],[['c',{lane:0,row:0}],['b',{lane:0,row:1}],['a',{lane:0,row:2}]]);
 assert.equal(result.lanes,1);
 assert.deepEqual(result.edges,[{id:'c>b',source:'c',target:'b'},{id:'b>a',source:'b',target:'a'}]);
});
test('a merge reserves distinct parent lanes and links both branches',()=>{
 const result=layoutGraph([commit('m',['a','b']),commit('b',['r']),commit('a',['r']),commit('r')]);
 assert.equal(result.lanes,2);
 assert.notEqual(result.positions.get('a').lane,result.positions.get('b').lane);
 assert.deepEqual(result.edges.filter(e=>e.source==='m').map(e=>e.target),['a','b']);
 assert.equal(result.positions.get('r').lane,0);
});
test('truncated parents get no edges and completed lanes can be reused',()=>{
 const result=layoutGraph([commit('tip',['root']),commit('root'),commit('other',['unloaded'])]);
 assert.equal(result.positions.get('other').lane,0);
 assert.deepEqual(result.edges,[{id:'tip>root',source:'tip',target:'root'}]);
});
test('refs distinguish HEAD, tags and common remote prefixes',()=>{
 assert.deepEqual(parseRefs(['HEAD -> feature/x','HEAD','tag: v1','origin/main','upstream/main','topic']),[
  {name:'feature/x',kind:'head'},{name:'HEAD',kind:'head'},{name:'v1',kind:'tag'},
  {name:'origin/main',kind:'remote'},{name:'upstream/main',kind:'remote'},{name:'topic',kind:'branch'},
 ]);
});
