import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source = readFileSync(new URL('../browser/jev-fast-path/content.js', import.meta.url), 'utf8');
function fixture(origin = 'https://www.netflix.com', pathname = '/browse') {
  let listener;
  const elements = [];
  const video = element('VIDEO', {});
  Object.assign(video, {paused:true, ended:false, readyState:4, currentTime:10});
  function element(tagName, attrs, href = '') {
    const e = {nodeType:1, tagName, href, parentElement:null, disabled:false,
      getAttribute: k => attrs[k] ?? null, hasAttribute:k => k in attrs,
      matches:s => s === 'a.slider-refocus' && attrs.class === 'slider-refocus',
      getBoundingClientRect:() => ({left:10,top:10,right:90,bottom:40,width:80,height:30,toJSON(){return this}}),
      contains:n => n === e, querySelector:() => ({alt:'A title'}), closest:() => null};
    return e;
  }
  const doc = {title:'A title', visibilityState:'visible', hasFocus:()=>true, documentElement:{},
    querySelectorAll:s=>s==='video' ? elements.filter(e=>e.tagName==='VIDEO') : elements, querySelector:()=>video, addEventListener(){},
    elementFromPoint:()=>elements[0], getElementById:()=>null};
  vm.runInNewContext(source, {document:doc,location:{origin,pathname,href:origin+pathname},URL,performance,
    browser:{runtime:{onMessage:{addListener:f=>listener=f},sendMessage(){}}},
    crypto:{randomUUID:()=> 'doc'},Node:{ELEMENT_NODE:1},
    HTMLInputElement:class{},HTMLTextAreaElement:class{},
    MutationObserver:class{observe(){} disconnect(){}},addEventListener(){},
    setTimeout:()=>1,clearTimeout(){},setInterval:()=>1,clearInterval(){},
    window:{mozInnerScreenX:0,mozInnerScreenY:0,devicePixelRatio:1},innerWidth:800,innerHeight:600,
    getComputedStyle:()=>({display:'block',visibility:'visible',opacity:'1'})});
  return {elements,video,element,snapshot(){let result;listener({type:'snapshot-request'},{},x=>result=x);return result.evidence.elements;}};
}
test('media adapter requires Netflix and exact title/play link routes', () => {
  const f=fixture();
  f.elements.push(f.element('A',{'data-uia':'play-button'},'https://www.netflix.com/watch/123'));
  assert.equal(f.snapshot()[0].role,'media play');
  f.elements[0].href='https://other.example/watch/123';assert.equal(f.snapshot().length,0);
  f.elements[0].href='https://www.netflix.com/account';assert.equal(f.snapshot().length,0);
  f.elements[0]=f.element('A',{class:'slider-refocus'},'https://www.netflix.com/browse?jbv=123');
  assert.equal(f.snapshot()[0].role,'media title');
  const other=fixture('https://other.example');
  other.elements.push(other.element('A',{'data-uia':'play-button'},'https://www.netflix.com/watch/123'));
  assert.ok(other.snapshot().every(e=>!e.role.startsWith('media')));
});
test('paused player exposes resume only and completion requires advancing video', () => {
  const f=fixture('https://www.netflix.com','/watch/123');f.elements.push(f.element('BUTTON',{'data-uia':'player-play-pause'}));
  assert.equal(f.snapshot()[0].role,'media play');
  f.video.paused=false;assert.equal(f.snapshot().length,0);
  f.elements[0]=f.video;assert.equal(f.snapshot().length,0);
  assert.equal(f.snapshot().length,0);
  f.video.currentTime=11;assert.equal(f.snapshot()[0].name,'Media playback active');
  f.video.paused=true;f.video.currentTime=12;assert.equal(f.snapshot().length,0);
  f.video.paused=false;f.video.ended=true;f.video.currentTime=13;assert.equal(f.snapshot().length,0);
});

test('preview videos do not establish episode playback completion', () => {
  const f=fixture();f.elements.push(f.video);f.video.paused=false;
  f.snapshot();f.video.currentTime=11;assert.equal(f.snapshot().length,0);
});

test('fractional video overflow is clipped for status but not for clicks', () => {
  const f=fixture('https://www.netflix.com','/watch/123');
  f.elements.push(f.video);f.video.paused=false;
  f.video.getBoundingClientRect=()=>({left:0,top:0,right:800.2,bottom:600.2,width:800.2,height:600.2});
  f.snapshot();f.video.currentTime=11;
  const status=f.snapshot()[0];assert.equal(status.width,800);assert.equal(status.height,600);
  const g=fixture();g.elements.push(g.element('A',{'data-uia':'play-button'},'https://www.netflix.com/watch/123'));
  g.elements[0].getBoundingClientRect=f.video.getBoundingClientRect;assert.equal(g.snapshot().length,0);
});
