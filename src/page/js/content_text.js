(()=>{const d=document;if(!d||!d.body)return'';
const SKIP={SCRIPT:1,STYLE:1,NOSCRIPT:1,SVG:1,TEMPLATE:1,LINK:1,META:1};
const isAd=function(n){var cl=(typeof n.className==='string'?n.className:'').toLowerCase();var id=(n.id||'').toLowerCase();
if(/dfp|advert|sponsored|ad-container|ad-wrapper|ad-slot|ad-banner|ad-feedback|adbanner|adsense|adblock|ad-label|ads-label|ads-container|mol-ads|promoted|google_ads|doubleclick/.test(cl))return true;
if(/dfp|advert|sponsored|google_ads|doubleclick/.test(id))return true;
try{if(n.hasAttribute('data-ad')||n.hasAttribute('data-ad-slot')||n.hasAttribute('data-ad-client')||n.hasAttribute('data-google-query-id'))return true;}catch(_e0){}
if(n.tagName==='INS'&&/adsbygoogle/.test(cl))return true;
var al=(n.getAttribute&&(n.getAttribute('aria-label')||'').toLowerCase())||'';if(al.indexOf('advertisement')>=0)return true;
return false;};
const parts=[];let cost=0;const CAP=__BUDGET__*3+400;let visits=0;const VMAX=20000;
const walk=function(n){if(cost>=CAP||visits>=VMAX)return;
if(n.nodeType===3){const t=n.textContent;if(t){parts.push(t);cost+=t.length;}return;}
if(n.nodeType!==1)return;
visits++;
const tag=n.tagName;
if(SKIP[tag])return;
if(n.hidden)return;
try{const cs=getComputedStyle(n);if(cs.display==='none'||cs.visibility==='hidden')return;}catch(_e1){}
if(isAd(n))return;
if(tag==='SELECT'){try{const ts=[...n.options].map(o=>(o.label||o.text||'').trim()).filter(Boolean).slice(0,12);if(ts.length){const extra=Math.max(0,n.options.length-ts.length);const s='['+ts.join(' | ')+(extra?' | +'+extra+' more':'')+'] ';parts.push(s);cost+=s.length;}}catch(_e2){}return;}
if(tag==='IFRAME'){try{const fd=n.contentDocument;if(fd&&fd.body)walk(fd.body);}catch(_e3){}return;}
const cs2=n.childNodes;for(let i=0;i<cs2.length;i++)walk(cs2[i]);
const sr=n.shadowRoot;if(sr){const sc=sr.childNodes;for(let i=0;i<sc.length;i++)walk(sc[i]);}
};
walk(d.body);
const t=parts.join(' ').replace(/\s+/g,' ').trim();
if(t.length>__BUDGET__)return t.slice(0,__BUDGET__)+'\n[truncated: '+t.length+' chars total, showed '+__BUDGET__+' - raise budget= for more]';
return t;})()
