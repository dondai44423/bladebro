// --- S10: proxy masks. Install helper contract: `fn(th, a, orig)` gets the
// call-site receiver, the arguments array, and the native target; it must
// delegate to `orig` (or a no-orig placeholder) so the native validation and
// coercion order run before any rewrite. ---
var _PP=typeof Proxy!=='undefined'?Proxy:null;
var _nop={m(){}}.m.bind(null);
function _mk(orig,fn){return new _PP(orig,{apply:function(t,th,a){return fn(th,a,t);}});}
function _mkN(fn){return new _PP(_nop,{apply:function(t,th,a){return fn(th,a,t);}});}
function _fix(p,name,len){try{Object.defineProperty(p,'name',{value:name,configurable:true});Object.defineProperty(p,'length',{value:len,configurable:true});}catch(e){}return p;}
function _ogs(obj,name){try{return Object.getOwnPropertyDescriptor(obj,name).get;}catch(e){return undefined;}}
// WebIDL attributes/methods are enumerable:true on prototypes; interface
// objects (constructors) on window are enumerable:false. A proxy of a native
// accessor keeps the engine's "get " name automatically.
function _defGet(obj,name,g,fn){if(!g)return;try{Object.defineProperty(obj,name,{get:_mk(g,fn),configurable:true,enumerable:true});}catch(e){}}
function _defGetN(obj,name,fn){try{Object.defineProperty(obj,name,{get:_fix(_mkN(fn),'get '+name,0),configurable:true,enumerable:true});}catch(e){}}
function _defFn(obj,name,orig,fn){if(!orig)return;try{Object.defineProperty(obj,name,{value:_mk(orig,fn),writable:true,configurable:true,enumerable:true});}catch(e){}}
function _defFnN(obj,name,fn,len){try{Object.defineProperty(obj,name,{value:_fix(_mkN(fn),name,len||0),writable:true,configurable:true,enumerable:true});}catch(e){}}
function _defCtor(name,fn){try{Object.defineProperty(window,name,{value:_fix(_mkN(fn),name,0),writable:true,configurable:true,enumerable:false});}catch(e){}}
