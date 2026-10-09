// Template extraction traversal + read contract (G06).
// __LISTS__ is substituted with the per-list code built from the template.
// Bounds: 20k-node walk budget, 48-frame cap; cross-origin/unloaded frames
// are counted (st.cross) and reported, never silently skipped.
(()=>{
const NODE_BUDGET=20000;
// Rendered check over the COMPOSED tree (crosses shadow hosts): an element
// hidden by display:none / visibility:hidden anywhere on its chain is not
// rendered. Same semantics the content readers use.
const rv=n=>{try{for(let c=n;c;c=(c.parentElement||(c.getRootNode&&c.getRootNode().host))){if(c.nodeType!==1)break;const s=getComputedStyle(c);if(s.display==='none'||s.visibility==='hidden')return false;}return true;}catch(e){return true;}};
// Light DOM + open shadow roots, same order as the model's walker.
const walk=(root,sel,st)=>{const out=[];const visit=c=>{if(st.hit)return;const m=c.querySelectorAll(sel);for(let i=0;i<m.length;i++)out.push(m[i]);const all=c.querySelectorAll('*');for(let i=0;i<all.length;i++){st.n++;if(st.n>=NODE_BUDGET){st.hit=true;break;}const sr=all[i].shadowRoot;if(sr)visit(sr);}};visit(root);return out;};
// Documents: this one + same-origin frames (bounded). contentDocument is
// null for cross-origin/unloaded frames - counted, never guessed at.
const collect=(d,sel,st)=>{const out=walk(d,sel,st);if(st.hit)return out;try{const ifs=d.querySelectorAll('iframe');for(let i=0;i<ifs.length;i++){if(st.f>=48)break;let fd=null;try{fd=ifs[i].contentDocument;}catch(e){}if(!fd){st.cross++;continue;}st.f++;const sub=collect(fd,sel,st);for(let j=0;j<sub.length;j++)out.push(sub[j]);if(st.hit)break;}}catch(e){}return out;};
// One field value. States stay distinct: omitted (rendered read of a hidden
// element) -> {om:true}; selector missed -> {v:null}; empty -> {v:""}.
const read=(c,sel,raw)=>{let s=sel,attr=null;const ai=sel.lastIndexOf('@');if(ai>0){attr=sel.slice(ai+1);s=sel.slice(0,ai);}let el=null;if(s){try{el=c.querySelector(s);}catch(e){}if(!el){const hits=walk(c,s,{n:0,hit:false});el=hits.length?hits[0]:null;}}else{el=c;}if(!el)return {v:null};if(attr)return {v:el.getAttribute(attr)};if(raw)return {v:String(el.innerText||el.textContent||'').trim()};if(!rv(el))return {om:true};const t=el.innerText;return {v:(t==null?'':String(t)).trim()};};
const st={n:0,f:0,cross:0,hit:false,om:0};
return {__LISTS__};
})()
