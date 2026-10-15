import {test} from 'node:test';
import assert from 'node:assert/strict';
import {layoutGraph,parseRefs,mergedPullRequests,legendOf,prEdgeColor,PR_COLORS} from './git-graph.ts';

const commit=(hash,parents=[],author='Test')=>({hash,parents,author,date:'2026-01-01',subject:hash,refs:[]});

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

// Newest first, like Git's topo order. Main line m5-m4-m3-m2-m1; f1,f2 (Ann, Bob) were merged by m3, g1 (Cy) by m5.
const history=()=>[
 commit('m5',['m4','g1']),commit('g1',['m3'],'Cy'),commit('m4',['m3']),
 commit('m3',['m2','f2']),commit('f2',['f1'],'Bob'),commit('f1',['m1'],'Ann'),commit('m2',['m1']),commit('m1'),
];
test('a merge on the main line yields its branch commits and the people who made them',()=>{
 const prs=mergedPullRequests(history(),'m5');
 const pr=prs.find(candidate=>candidate.merge==='m3');
 assert.deepEqual({members:pr.members,authors:pr.authors,key:pr.key},{members:['f2','f1'],authors:['Ann','Bob'],key:'Ann\nBob'});
});
test('colours follow the set of people, in first-seen order from the newest merge',()=>{
 assert.deepEqual(mergedPullRequests(history(),'m5').map(pr=>[pr.merge,pr.key,pr.color]),[['m5','Cy',PR_COLORS[0]],['m3','Ann\nBob',PR_COLORS[1]]]);
 const merged=mergedPullRequests([commit('m6',['m5','k2']),commit('k2',['k1'],'Bob'),commit('k1',['m5'],'Ann'),...history()],'m6');
 assert.deepEqual(merged.map(pr=>pr.merge),['m6','m5','m3']);
 assert.equal(merged[0].color,PR_COLORS[0]);
 assert.equal(merged[2].color,PR_COLORS[0],'the same people share a colour');
});
test('only merges on the default branch line are pull requests',()=>{
 const side=[commit('tip',['m2']),commit('side',['s1','s2']),commit('s2',['m1'],'Bob'),commit('s1',['m1'],'Ann'),commit('m2',['m1']),commit('m1')];
 assert.deepEqual(mergedPullRequests(side,'tip'),[],'a merge reachable only from another branch is not shown');
 assert.deepEqual(mergedPullRequests([commit('m3',['m2','m1']),commit('m2',['m1']),commit('m1')],'m3'),[],'a second parent that is already contained brings nothing');
 assert.deepEqual(mergedPullRequests(history(),null),[]);
});
test('the legend lists each set of people once and truncates past three names',()=>{
 const chain=authors=>[commit('m',['p','a0']),...authors.map((author,i)=>commit(`a${i}`,[i+1<authors.length?`a${i+1}`:'p'],author)),commit('p')];
 const [many]=legendOf(mergedPullRequests(chain(['A','B','C','D']),'m'));
 assert.deepEqual([many.names,many.more,many.label],[['A','B','C'],true,'A, B, C, ...']);
 assert.equal(legendOf(mergedPullRequests(chain(['A','B','C']),'m'))[0].label,'A, B, C');
 assert.equal(legendOf(mergedPullRequests(history(),'m5')).length,2);
 const same=[commit('m6',['m5','k2']),commit('k2',['k1'],'Bob'),commit('k1',['m5'],'Ann'),...history()];
 assert.equal(legendOf(mergedPullRequests(same,'m6')).length,2,'m6 and m3 are by the same people');
});
test('edges inside a pull request branch and from its merge to its tip are coloured',()=>{
 const color=prEdgeColor(mergedPullRequests(history(),'m5'));
 assert.equal(color('m5','g1'),PR_COLORS[0]);
 assert.equal(color('f2','f1'),PR_COLORS[1]);
 assert.equal(color('m3','f2'),PR_COLORS[1]);
 assert.equal(color('m5','m4'),null);
 assert.equal(color('g1','m3'),null,'the fork point is on the main line');
});
