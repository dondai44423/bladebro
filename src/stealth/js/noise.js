
// Seeded canvas noise (stable per session — random-per-load is itself a signal).
try{
var _otd=HTMLCanvasElement.prototype.toDataURL;var _ogi=CanvasRenderingContext2D.prototype.getImageData;var _m=new WeakMap();
_defFn(HTMLCanvasElement.prototype,'toDataURL',_otd,function(th,a,og){
  if(!_m.has(th)){try{var c=th.getContext('2d');if(c&&th.width>0&&th.height>0){var px=_ogi.call(c,0,0,1,1);px.data[0]=(px.data[0]+cn)%256;px.data[1]=(px.data[1]+cn2)%256;c.putImageData(px,0,0);_m.set(th,true);}}catch(e){}}
  return og.apply(th,a);
});
}catch(e){}

// Seeded audio noise.
try{
var _ogcd=AudioBuffer.prototype.getChannelData;
_defFn(AudioBuffer.prototype,'getChannelData',_ogcd,function(th,a,og){var d=og.apply(th,a);if(d.length>0){d[0]+=an*1e-7;}return d;});
}catch(e){}

