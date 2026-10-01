
// mediaDevices: a machine with zero audio/video devices is a server tell.
try{
var _mdp=(typeof MediaDevices!=='undefined'&&MediaDevices.prototype)?MediaDevices.prototype:Object.getPrototypeOf(navigator.mediaDevices);
if(_mdp){
  var _ed=_mdp.enumerateDevices;
  _defFn(_mdp,'enumerateDevices',_ed,function(th,a,og){
    return og.apply(th,a).then(function(d){
      if(d&&d.length>0)return d;
      function mk(k){var o={deviceId:'',kind:k,label:'',groupId:''};o.toJSON=_fix(_mkN(function(){return{deviceId:'',kind:k,label:'',groupId:''};}),'toJSON',0);return o;}
      return[mk('audioinput'),mk('audiooutput')];
    });
  });
}
}catch(e){}

