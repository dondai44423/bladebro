// --- S10: proxy masks. Install helper contract: `fn(th, a, orig)` gets the
// call-site receiver, the arguments array, and the native target; it must
// delegate to `orig` (or a no-orig placeholder) so the native validation and
// coercion order run before any rewrite. ---
// Every mask is a Proxy over a native target: the proxy stringifies as
// "[native code]" in every realm, keeps {length,name}, has no own
// 'prototype', and forwards receiver/argument semantics to the native.
//
// The one shape a bare callable Proxy could not reproduce (4.6.0 fix): V8's
// cycle check on `Object.setPrototypeOf(fn, Object.create(fn))` compares the
// requested prototype chain against the proxy's TARGET, never the proxy
// itself, so the assignment succeeded and the follow-up coercion recursed
// (RangeError) where a real function throws TypeError('Cyclic __proto__ value')
// from setPrototypeOf. creepjs tests/prototype.html reads exactly that
// difference ('failed at too much recursion error') — it was the single lie
// the agent lane left on every masked property (measured 2026-10-14). The
// `setPrototypeOf` trap below performs the missing check against the PROXY —
// object AND function protos (4.6.1: the function-typed torture escaped an
// object-only guard and recursed to RangeError, measured in work/stealth461) —
// and throws the engine's own cycle TypeError; every non-cyclic assignment
// forwards
// to the target untouched. The trap fires only on explicit setPrototypeOf
// calls — never on the call path — so masked calls keep native speed. (The
// cyclic-Reflect and `__proto__`-self-assign paths of the same torture family
// remain engine-level residual, reachable only after a page already flagged
// Function.toString — see status.md 4.6.0.)
var _PP=typeof Proxy!=='undefined'?Proxy:null;
var _nop={m(){}}.m.bind(null);
function _cyc(pp,p){
  if(p===null)return null;
  var t=typeof p;if(t!=='object'&&t!=='function')return null;
  try{
    var q=p,i=0;
    while(q&&i++<256){if(q===pp)return new TypeError('Cyclic __proto__ value');q=Object.getPrototypeOf(q);}
  }catch(e){}
  return null;
}
function _mk(orig,fn){var h={apply:function(t,th,a){return fn(th,a,t);}};var pp=new _PP(orig,h);h.setPrototypeOf=function(t,p){var e=_cyc(pp,p);if(e)throw e;return Reflect.setPrototypeOf(t,p);};return pp;}
function _mkN(fn){var h={apply:function(t,th,a){return fn(th,a,t);}};var pp=new _PP(_nop,h);h.setPrototypeOf=function(t,p){var e=_cyc(pp,p);if(e)throw e;return Reflect.setPrototypeOf(t,p);};return pp;}
function _fix(p,name,len){try{Object.defineProperty(p,'name',{value:name,configurable:true});Object.defineProperty(p,'length',{value:len,configurable:true});}catch(e){}return p;}
function _ogs(obj,name){try{return Object.getOwnPropertyDescriptor(obj,name).get;}catch(e){return undefined;}}
// WebIDL attributes/methods are enumerable:true on prototypes; interface
// objects (constructors) on window are enumerable:false. A proxy of a native
// accessor keeps the engine's "get " name automatically.
function _defGet(obj,name,g,fn){if(!g)return;try{Object.defineProperty(obj,name,{get:_mk(g,fn),configurable:true,enumerable:true});}catch(e){}}
function _defGetN(obj,name,fn){try{Object.defineProperty(obj,name,{get:_fix(_mkN(fn),'get '+name,0),configurable:true,enumerable:true});}catch(e){}}
function _defFn(obj,name,orig,fn){if(!orig)return;try{Object.defineProperty(obj,name,{value:_mk(orig,fn),writable:true,configurable:true,enumerable:true});}catch(e){}}
function _defFnN(obj,name,fn,len){try{Object.defineProperty(obj,name,{value:_fix(_mkN(fn),name,len||0),writable:true,configurable:true,enumerable:true});}catch(e){}}
