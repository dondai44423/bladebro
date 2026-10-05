
try{
if(typeof RTCPeerConnection!=='undefined'){
  var _origRTC=RTCPeerConnection.prototype;
  // Leaky = names a real address: srflx (STUN-reflexive = the egress IP) or
  // a raw-IP host candidate. mDNS `.local` host candidates are obfuscated
  // and pass; relay and prflx pass.
  function _leakyCand(c){if(!c)return false;if(c.indexOf('typ srflx')!==-1)return true;if(c.indexOf('typ host')!==-1&&c.indexOf('.local')===-1)return true;return false;}
  function _filterSDP(sdp){
    if(!sdp)return sdp;
    return sdp.replace(/a=candidate:[^\r\n]*typ host[^\r\n]*/g,'').replace(/a=candidate:[^\r\n]*typ srflx[^\r\n]*/g,'');
  }
  var _oco=_origRTC.createOffer,_oca=_origRTC.createAnswer,_oaic=_origRTC.addIceCandidate;
  _defFn(_origRTC,'createOffer',_oco,function(th,a,og){
    return og.apply(th,a).then(function(offer){
      if(offer&&offer.sdp){offer.sdp=_filterSDP(offer.sdp);}
      return offer;
    });
  });
  _defFn(_origRTC,'createAnswer',_oca,function(th,a,og){
    return og.apply(th,a).then(function(answer){
      if(answer&&answer.sdp){answer.sdp=_filterSDP(answer.sdp);}
      return answer;
    });
  });
  _defFn(_origRTC,'addIceCandidate',_oaic,function(th,a,og){
    var c=a[0];
    if(c&&c.candidate&&String(c.candidate).indexOf('typ host')!==-1){return Promise.resolve();}
    return og.apply(th,a);
  });
  // Local candidate events — the page's own enumeration vector. Installed at
  // the native locations (EventTarget.prototype already owns add/removeEvent
  // Listener; proxy over the native keeps the descriptor shape and the
  // native-shaped toString) and scoped to RTCPeerConnection receivers.
  var _ael=EventTarget.prototype.addEventListener,_rel=EventTarget.prototype.removeEventListener;
  _defFn(EventTarget.prototype,'addEventListener',_ael,function(th,a,og){
    if(a[0]==='icecandidate'&&typeof a[1]==='function'&&!a[1].__ocWrap&&typeof RTCPeerConnection!=='undefined'&&th instanceof RTCPeerConnection){
      var fn=a[1];
      var wrap=function(ev){if(ev&&ev.candidate&&_leakyCand(ev.candidate.candidate))return;return fn.apply(this,arguments);};
      wrap.__ocWrap=fn;a=Array.prototype.slice.call(a);a[1]=wrap;
    }
    return og.apply(th,a);
  });
  _defFn(EventTarget.prototype,'removeEventListener',_rel,function(th,a,og){
    if(a[0]==='icecandidate'&&typeof a[1]==='function'&&a[1].__ocWrap){a=Array.prototype.slice.call(a);a[1]=a[1].__ocWrap;}
    return og.apply(th,a);
  });
  // onicecandidate handler property (own accessor on the prototype, the
  // native location). The WeakMap keeps `pc.onicecandidate === fn` true for
  // whatever the page set.
  try{
    var _ocd=Object.getOwnPropertyDescriptor(_origRTC,'onicecandidate');
    if(_ocd&&_ocd.get&&_ocd.set){
      var _ocmap=new WeakMap();
      var _ocget=function(th,a,og){var f=_ocmap.get(th);return f!==undefined?f:og.call(th);};
      var _ocset=function(th,a,og){
        var fn=a[0];
        if(typeof fn!=='function'){_ocmap.delete(th);return og.call(th,fn);}
        _ocmap.set(th,fn);
        return og.call(th,function(ev){if(ev&&ev.candidate&&_leakyCand(ev.candidate.candidate))return;return fn.call(this,ev);});
      };
      Object.defineProperty(_origRTC,'onicecandidate',{configurable:true,enumerable:!!_ocd.enumerable,get:_mk(_ocd.get,_ocget),set:_mk(_ocd.set,_ocset)});
    }
  }catch(e){}
}
}catch(e){}

