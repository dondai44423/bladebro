
// mediaDevices: a machine with zero audio/video devices is a server tell.
try{
var _mdp=(typeof MediaDevices!=='undefined'&&MediaDevices.prototype)?MediaDevices.prototype:Object.getPrototypeOf(navigator.mediaDevices);
if(_mdp){
  var _ed=_mdp.enumerateDevices;
  var _mdc=_ogs(_mdp,'ondevicechange');
  if(_mdc)_defFn(_mdp,'enumerateDevices',_ed,function(th,a,og){
    var p=Reflect.apply(og,th,a);
    // An invalid receiver already produced a rejected native promise. Do not
    // add a .then hop: its rejection would move behind the caller's microtask.
    // This native getter checks the brand without reading caller properties.
    try{Reflect.apply(_mdc,th,[]);}catch(e){return p;}
    return p.then(function(d){
      if(d&&d.length>0)return d;
      function mk(k){var o={deviceId:'',kind:k,label:'',groupId:''};o.toJSON=_fix(_mkN(function(){return{deviceId:'',kind:k,label:'',groupId:''};}),'toJSON',0);return o;}
      return[mk('audioinput'),mk('audiooutput')];
    });
  });
}
}catch(e){}

