
// cdc_ late-injection watcher (first 3s only, then disconnects).
// Throttled: getOwnPropertyNames on EVERY mutation was measurable
// main-thread jank in the first 3s — itself a timing fingerprint.
var _cdcLast=0;
var obs=new MutationObserver(function(){
  var now=Date.now();if(now-_cdcLast<500)return;_cdcLast=now;
  var q=Object.getOwnPropertyNames(document);
  for(var j=0;j<q.length;j++){if(q[j].indexOf('cdc_')===0){try{delete document[q[j]];}catch(e){}}}
});obs.observe(document,{childList:true,subtree:true});setTimeout(function(){obs.disconnect();},3000);
})();