(function(){
var out={};
var IS_BLINK=true,IS_WEBKIT=false,IS_GECKO=false,IS_WORKER_SCOPE=false;
var HAS_REFLECT='Reflect' in self;
function isTypeError(err){return err.constructor.name=='TypeError';}
function failsTypeError(t){var spawnErr=t.spawnErr,withStack=t.withStack,final=t.final;try{spawnErr();throw Error();}catch(err){if(!isTypeError(err))return true;return withStack?withStack(err):false;}finally{if(final)final();}}
function failsWithError(fn){try{fn();return false}catch(e){return true}}
function hasKnownToString(name){var o={};o['function '+name+'() { [native code] }']=true;o['function get '+name+'() { [native code] }']=true;o['function () { [native code] }']=true;o['function '+name+'() {\n    [native code]\n}']=true;o['function get '+name+'() {\n    [native code]\n}']=true;o['function () {\n    [native code]\n}']=true;return o;}
function hasValidStack(err,reg,i){if(i===undefined)i=1;if(i===0)return reg.test(err.message);return reg.test(String(err.stack||'').split('\n')[i]);}
var AT_FUNCTION=/at Function\.toString /;
var AT_OBJECT=/at Object\.toString/;
var FUNCTION_INSTANCE=/at (Function\.)?\[Symbol\.hasInstance\]/;
var PROXY_INSTANCE=/at (Proxy\.)?\[Symbol\.hasInstance\]/;
var STRICT_MODE=/strict mode/;
var RAND='r'+Math.random().toString(36).slice(-7);
function queryLies(cfg){
  var scope=cfg.scope,apiFunction=cfg.apiFunction,proto=cfg.proto,obj=cfg.obj,lieProps=cfg.lieProps;
  if(typeof apiFunction!='function')return{lied:0,lieTypes:[]};
  var name=apiFunction.name.replace(/get\s/,'');
  var objName=obj&&obj.name;
  var nativeProto=Object.getPrototypeOf(apiFunction);
  var lies={};
  lies['failed illegal error']=!!obj&&failsTypeError({spawnErr:function(){return obj.prototype[name];}});
  lies['failed undefined properties']=(!!obj&&/^(screen|navigator)$/i.test(objName)&&!!(Object.getOwnPropertyDescriptor(self[objName.toLowerCase()],name)||(HAS_REFLECT&&Reflect.getOwnPropertyDescriptor(self[objName.toLowerCase()],name))));
  lies['failed call interface error']=failsTypeError({spawnErr:function(){new apiFunction();apiFunction.call(proto);}});
  lies['failed apply interface error']=failsTypeError({spawnErr:function(){new apiFunction();apiFunction.apply(proto);}});
  lies['failed new instance error']=failsTypeError({spawnErr:function(){return new apiFunction();}});
  lies['failed class extends error']=(!IS_WEBKIT&&failsTypeError({spawnErr:function(){class Fake extends apiFunction{}}}));
  lies['failed null conversion error']=failsTypeError({spawnErr:function(){return Object.setPrototypeOf(apiFunction,null).toString();},final:function(){Object.setPrototypeOf(apiFunction,nativeProto);}});
  lies['failed toString']=(!hasKnownToString(name)[scope.Function.prototype.toString.call(apiFunction)]||!hasKnownToString('toString')[scope.Function.prototype.toString.call(apiFunction.toString)]);
  lies['failed "prototype" in function']='prototype' in apiFunction;
  lies['failed descriptor']=!!(Object.getOwnPropertyDescriptor(apiFunction,'arguments')||Reflect.getOwnPropertyDescriptor(apiFunction,'arguments')||Object.getOwnPropertyDescriptor(apiFunction,'caller')||Reflect.getOwnPropertyDescriptor(apiFunction,'caller')||Object.getOwnPropertyDescriptor(apiFunction,'prototype')||Reflect.getOwnPropertyDescriptor(apiFunction,'prototype')||Object.getOwnPropertyDescriptor(apiFunction,'toString')||Reflect.getOwnPropertyDescriptor(apiFunction,'toString'));
  lies['failed own property']=!!(apiFunction.hasOwnProperty('arguments')||apiFunction.hasOwnProperty('caller')||apiFunction.hasOwnProperty('prototype')||apiFunction.hasOwnProperty('toString'));
  lies['failed descriptor keys']=(Object.keys(Object.getOwnPropertyDescriptors(apiFunction)).sort().toString()!='length,name');
  lies['failed own property names']=(Object.getOwnPropertyNames(apiFunction).sort().toString()!='length,name');
  lies['failed own keys names']=HAS_REFLECT&&(Reflect.ownKeys(apiFunction).sort().toString()!='length,name');
  lies['failed object toString error']=(failsTypeError({spawnErr:function(){return Object.create(apiFunction).toString();},withStack:function(err){return IS_BLINK&&!hasValidStack(err,AT_FUNCTION);}})||failsTypeError({spawnErr:function(){return Object.create(new Proxy(apiFunction,{})).toString();},withStack:function(err){return IS_BLINK&&!hasValidStack(err,AT_OBJECT);}}));
  lies['failed at incompatible proxy error']=failsTypeError({spawnErr:function(){apiFunction.arguments;apiFunction.caller;},withStack:function(err){return IS_GECKO&&!hasValidStack(err,STRICT_MODE,0);}});
  lies['failed at toString incompatible proxy error']=failsTypeError({spawnErr:function(){apiFunction.toString.arguments;apiFunction.toString.caller;},withStack:function(err){return IS_GECKO&&!hasValidStack(err,STRICT_MODE,0);}});
  lies['failed at too much recursion error']=failsTypeError({spawnErr:function(){return Object.setPrototypeOf(apiFunction,Object.create(apiFunction)).toString();},final:function(){Object.setPrototypeOf(apiFunction,nativeProto);}});
  var detectProxies=(name=='toString'||!!lieProps['Function.toString']||!!lieProps['Permissions.query']);
  if(detectProxies){
    var proxy1=new Proxy(apiFunction,{});
    var proxy2=new Proxy(apiFunction,{});
    var proxy3=new Proxy(apiFunction,{});
    var proxy=proxy1;
    lies['failed at too much recursion __proto__ error']=!failsTypeError({spawnErr:function(){apiFunction.__proto__=proxy;apiFunction++;},final:function(){Object.setPrototypeOf(apiFunction,nativeProto);}});
    lies['failed at chain cycle error']=!failsTypeError({spawnErr:function(){Object.setPrototypeOf(proxy1,Object.create(proxy1)).toString();},final:function(){Object.setPrototypeOf(proxy1,nativeProto);}});
    lies['failed at chain cycle __proto__ error']=!failsTypeError({spawnErr:function(){proxy2.__proto__=proxy2;proxy2++;},final:function(){Object.setPrototypeOf(proxy2,nativeProto);}});
    lies['failed at reflect set proto']=HAS_REFLECT&&failsTypeError({spawnErr:function(){Reflect.setPrototypeOf(apiFunction,Object.create(apiFunction));RAND in apiFunction;throw new TypeError();},final:function(){Object.setPrototypeOf(apiFunction,nativeProto);}});
    lies['failed at reflect set proto proxy']=HAS_REFLECT&&!failsTypeError({spawnErr:function(){Reflect.setPrototypeOf(proxy3,Object.create(proxy3));RAND in proxy3;},final:function(){Object.setPrototypeOf(proxy3,nativeProto);}});
    lies['failed at instanceof check error']=IS_BLINK&&(failsTypeError({spawnErr:function(){apiFunction instanceof apiFunction;},withStack:function(err){return !hasValidStack(err,FUNCTION_INSTANCE);}})||failsTypeError({spawnErr:function(){var p=new Proxy(apiFunction,{});p instanceof p;},withStack:function(err){return !hasValidStack(err,PROXY_INSTANCE);}}));
    lies['failed at define properties']=IS_BLINK&&HAS_REFLECT&&failsWithError(function(){Object.defineProperty(apiFunction,'',{configurable:true}).toString();Reflect.deleteProperty(apiFunction,'');});
  }
  var lieTypes=Object.keys(lies).filter(function(k){return !!lies[k];});
  return {lied:lieTypes.length,lieTypes:lieTypes};
}
function createLieDetector(scope){
  var props={};
  var propsSearched=[];
  function isSupported(obj){return typeof obj!='undefined'&&!!obj;}
  function searchLies(fn,config){
    var target=config&&config.target,ignore=config&&config.ignore;
    var obj;
    try{obj=fn();if(!isSupported(obj))return;}catch(error){return;}
    var interfaceObject=obj.prototype?obj.prototype:obj;
    var names=[];
    Object.getOwnPropertyNames(interfaceObject).forEach(function(n){if(names.indexOf(n)<0)names.push(n)});
    Object.keys(interfaceObject).forEach(function(n){if(names.indexOf(n)<0)names.push(n)});
    names.sort().forEach(function(name){
      var skip=(name=='constructor'||(target&&target.indexOf(name)<0)||(ignore&&ignore.indexOf(name)>=0));
      if(skip)return;
      var objectNameString=/\s(.+)\]/;
      var apiName=(obj.name?obj.name:(objectNameString.test(obj)?objectNameString.exec(obj)[1]:undefined))+'.'+name;
      propsSearched.push(apiName);
      try{
        var proto=obj.prototype?obj.prototype:obj;
        var res;
        try{
          var apiFunction=proto[name];
          if(typeof apiFunction=='function'){
            res=queryLies({scope:scope,apiFunction:proto[name],proto:proto,obj:null,lieProps:props});
            if(res.lied){props[apiName]=res.lieTypes;}
            return;
          }
          if(name!='name'&&name!='length'&&name[0]!==name[0].toUpperCase()){
            props[apiName]=['failed descriptor.value undefined'];
            return;
          }
        }catch(error){}
        var getterFunction=Object.getOwnPropertyDescriptor(proto,name).get;
        if(!getterFunction)return;
        res=queryLies({scope:scope,apiFunction:getterFunction,proto:proto,obj:obj,lieProps:props});
        if(res.lied){props[apiName]=res.lieTypes;}
        return;
      }catch(error){
        props[apiName]=['failed prototype test execution'];
        return;
      }
    });
  }
  return {getProps:function(){return props;},getPropsSearched:function(){return propsSearched;},searchLies:searchLies};
}
// phantom realm as creepjs builds it (nested iframes)
var phantom=(function(){
  try{
    var div=document.createElement('div');
    div.setAttribute('style','height:100vh;width:100vw;position:absolute;left:-10000px;visibility:hidden;');
    div.innerHTML='<div><iframe></iframe></div>';
    document.body.appendChild(div);
    var iw=div.firstChild.firstChild.contentWindow;
    var div2=iw.document.createElement('div');
    div2.innerHTML='<div><iframe></iframe></div>';
    iw.document.body.appendChild(div2);
    return div2.firstChild.firstChild.contentWindow;
  }catch(e){return self;}
})();
var d=createLieDetector(phantom);
var S=d.searchLies;
S(function(){return Function},{target:['toString'],ignore:['caller','arguments']});
[
 [function(){return AnalyserNode}],
 [function(){return AudioBuffer},{target:['copyFromChannel','getChannelData']}],
 [function(){return BiquadFilterNode},{target:['getFrequencyResponse']}],
 [function(){return CanvasRenderingContext2D},{target:['getImageData','getLineDash','isPointInPath','isPointInStroke','measureText','quadraticCurveTo','fillText','strokeText','font']}],
 [function(){return CSSStyleDeclaration},{target:['setProperty']}],
 [function(){return Date},{target:['getDate','getDay','getFullYear','getHours','getMinutes','getMonth','getTime','getTimezoneOffset','setDate','setFullYear','setHours','setMilliseconds','setMonth','setSeconds','setTime','toDateString','toJSON','toLocaleDateString','toLocaleString','toLocaleTimeString','toString','toTimeString','valueOf']}],
 [function(){return GPU},{target:['requestAdapter']}],
 [function(){return GPUAdapter},{target:['requestAdapterInfo']}],
 [function(){return Intl.DateTimeFormat},{target:['format','formatRange','formatToParts','resolvedOptions']}],
 [function(){return Document},{target:['createElement','createElementNS','getElementById','getElementsByClassName','getElementsByName','getElementsByTagName','getElementsByTagNameNS','referrer','write','writeln'],ignore:['onreadystatechange','onmouseenter','onmouseleave']}],
 [function(){return DOMRect}],
 [function(){return DOMRectReadOnly}],
 [function(){return Element},{target:['append','appendChild','getBoundingClientRect','getClientRects','insertAdjacentElement','insertAdjacentHTML','insertAdjacentText','insertBefore','prepend','replaceChild','replaceWith','setAttribute']}],
 [function(){return FontFace},{target:['family','load','status']}],
 [function(){return HTMLCanvasElement}],
 [function(){return HTMLElement},{target:['clientHeight','clientWidth','offsetHeight','offsetWidth','scrollHeight','scrollWidth'],ignore:['onmouseenter','onmouseleave']}],
 [function(){return HTMLIFrameElement},{target:['contentDocument','contentWindow']}],
 [function(){return IntersectionObserverEntry},{target:['boundingClientRect','intersectionRect','rootBounds']}],
 [function(){return Math},{target:['acos','acosh','asinh','atan','atan2','atanh','cbrt','cos','cosh','exp','expm1','log','log10','log1p','sin','sinh','sqrt','tan','tanh']}],
 [function(){return MediaDevices},{target:['enumerateDevices','getDisplayMedia','getUserMedia']}],
 [function(){return Navigator},{target:['appCodeName','appName','appVersion','buildID','connection','deviceMemory','getBattery','getGamepads','getVRDisplays','hardwareConcurrency','language','languages','maxTouchPoints','mimeTypes','oscpu','platform','plugins','product','productSub','sendBeacon','serviceWorker','storage','userAgent','vendor','vendorSub','webdriver','gpu']}],
 [function(){return Node},{target:['appendChild','insertBefore','replaceChild']}],
 [function(){return OffscreenCanvas},{target:['convertToBlob','getContext']}],
 [function(){return Permissions},{target:['query']}],
 [function(){return Range},{target:['getBoundingClientRect','getClientRects']}],
 [function(){return Screen}],
 [function(){return speechSynthesis},{target:['getVoices']}],
 [function(){return String},{target:['fromCodePoint']}],
 [function(){return StorageManager},{target:['estimate']}],
 [function(){return SVGRect}],
 [function(){return SVGRectElement},{target:['getBBox']}],
 [function(){return SVGTextContentElement},{target:['getExtentOfChar','getSubStringLength','getComputedTextLength']}],
 [function(){return TextMetrics}],
 [function(){return WebGLRenderingContext},{target:['bufferData','getParameter','readPixels']}],
 [function(){return WebGL2RenderingContext},{target:['bufferData','getParameter','readPixels']}]
].forEach(function(cfg){try{S(cfg[0],cfg[1])}catch(e){}});
var props=d.getProps();
var getNonFunctionToStringLies=function(x){return !x?x:x.filter(function(v){return !/object toString|toString incompatible proxy/.test(v)}).length};
var lieProps={};
Object.keys(props).forEach(function(k){lieProps[k]=getNonFunctionToStringLies(props[k])});
out.searched=d.getPropsSearched().length;
out.corrupted=Object.keys(props).length;
out.flags={
  hasToStringProxy:!!lieProps['Function.toString'],
  webDriverIsOn_lieClause:!!lieProps['Navigator.webdriver'],
  lieProps_truthy:Object.keys(lieProps).filter(function(k){return !!lieProps[k]})
};
out.detail=props;
return JSON.stringify(out,null,1);
})()
