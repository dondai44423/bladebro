
try{
var _opq=navigator.permissions.query;
var _pp=Object.getPrototypeOf(navigator.permissions);
var _pq=function(th,a,og){
  var p=og.apply(th,a);
  try{
    var d=a.length>0?a[0]:undefined;
    if(d&&typeof d==='object'&&d.name==='notifications'&&location.origin!=='null'){
      return p.then(function(s){
        if(s&&s.state==='denied'){try{Object.defineProperty(s,'state',{value:'prompt',configurable:true});}catch(e){}}
        return s;
      });
    }
  }catch(e){}
  return p;
};
_defFn(_pp,'query',_pp.query||_opq,_pq);
}catch(e){}

