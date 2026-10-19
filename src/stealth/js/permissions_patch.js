// Leave query itself native. Reading descriptor.name after delegation invokes
// caller getters twice; chaining its promise also changes rejection scheduling.
// Mask only the native status getter, after native receiver validation, without
// touching arguments, returned promises, or the status instance's own keys.
try{
if(typeof PermissionStatus!=='undefined'){
  var _psp=PermissionStatus.prototype;
  var _psn=_ogs(_psp,'name');
  var _pss=_ogs(_psp,'state');
  if(_psn&&_pss)_defGet(_psp,'state',_pss,function(th,a,og){
    var s=Reflect.apply(og,th,a);
    if(s==='denied'&&Reflect.apply(_psn,th,[])==='notifications'&&location.origin!=='null')return 'prompt';
    return s;
  });
}
}catch(e){}
