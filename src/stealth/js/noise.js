
// Seeded canvas noise (stable per session — random-per-load is itself a signal).
// The offset goes through the canvas's EXISTING 2D context only: getContext('2d')
// on a canvas that has none creates one and locks the element to 2D (a later
// getContext('webgl') returns null — the old toDataURL broke WebP/feature probes
// that reuse their canvas). A 1x1 copy of pixel (0,0) into a private scratch
// canvas decides first: content there proves the canvas already has a context,
// and a transparent pixel would absorb the offset anyway (premultiplied alpha).
// The native call runs first: receiver and tainted-canvas errors stay native,
// and a tainted canvas never reaches (and taints) the scratch. So the first
// noised call on a canvas encodes twice (before and after the offset).
// A canvas is settled (_m) once noised, or once getContext('2d') returns null:
// its context is WebGL/bitmaprenderer, fixed for the element's lifetime, so
// later calls skip the 1x1 probe.
try{
var _otd=HTMLCanvasElement.prototype.toDataURL;var _ogi=CanvasRenderingContext2D.prototype.getImageData;var _m=new WeakMap();
var _odi=CanvasRenderingContext2D.prototype.drawImage,_ocr=CanvasRenderingContext2D.prototype.clearRect;
// Created now, before page scripts run, so no page hook sees it.
var _nzs=document.createElement('canvas');_nzs.width=1;_nzs.height=1;var _nzc=_nzs.getContext('2d',{willReadFrequently:true});
_defFn(HTMLCanvasElement.prototype,'toDataURL',_otd,function(th,a,og){
  var r=og.apply(th,a);
  if(!_m.has(th)){try{if(th.width>0&&th.height>0){
    _ocr.call(_nzc,0,0,1,1);_odi.call(_nzc,th,0,0,1,1,0,0,1,1);
    if(_ogi.call(_nzc,0,0,1,1).data[3]>0){var c=th.getContext('2d');if(c){var px=_ogi.call(c,0,0,1,1);px.data[0]=(px.data[0]+cn)%256;px.data[1]=(px.data[1]+cn2)%256;c.putImageData(px,0,0);_m.set(th,true);r=og.apply(th,a);}else _m.set(th,true);}
  }}catch(e){}}
  return r;
});
}catch(e){}

// Seeded audio noise, once per channel array: getChannelData hands back the
// same live Float32Array on every call, so adding on each call drifted the
// real samples without bound (and broke the stability the seed exists for).
try{
var _ogcd=AudioBuffer.prototype.getChannelData;var _ad=new WeakSet();
_defFn(AudioBuffer.prototype,'getChannelData',_ogcd,function(th,a,og){var d=og.apply(th,a);if(d.length>0&&!_ad.has(d)){_ad.add(d);d[0]+=an*1e-7;}return d;});
}catch(e){}
