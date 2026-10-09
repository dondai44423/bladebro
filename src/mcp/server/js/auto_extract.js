(()=>{
// Price: currency symbol required (no bare decimal numbers — false positives).
const PRICE=/([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/;
const PROD_URL=/\/dp\/|\/gp\/product\/|\/product\/|\/itm\/|\/products\/|\/p\//;
const DATE=/(\b\d{4}-\d{2}-\d{2}\b|\b\d{1,2}[\/]\d{1,2}[\/]\d{2,4}\b|\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+\d{1,2},?\s*\d{2,4}\b|\b\d+\s+(?:second|minute|hour|day|week|month|year)s?\s+ago\b)/i;
const HOST=location.hostname;const IS_REDDIT=HOST.includes('reddit.com');const IS_GITHUB=HOST==='github.com';function parseCount(s){s=s.trim();let n=parseFloat(s.replace(/,/g,''));if(/k$/i.test(s))n*=1000;else if(/m$/i.test(s))n*=1000000;return Math.round(n);}
const ACT_PAT=/^(vote|upvote|downvote|comment|comments|discuss|reply|replies|share|save|hide|flag|report|favorite|fav|like|dislike|follow|Subscribe|Pin|Unpin|More|less|edit|delete|remove|add|new|open|show|expand|collapse|permalink|embed|cite|parent|context|full story|read more|continue reading|view|all|next|prev|previous)$/i;
const ACT_HREF=/\/vote|\/comment|\/reply|\/action|javascript:|#comment|#respond|#reply/i;
function sig(el){const k=[...el.children].map(c=>c.tagName).join(',');return el.tagName+'['+k+']';}
function txt(el){return(el.innerText||el.textContent||'').replace(/\s+/g,' ').trim();}
function vtxt(el){return(el.innerText||'').replace(/\s+/g,' ').trim();}
function links(el){const r=[...el.querySelectorAll('a[href]')];if(el.matches&&el.matches('a[href]'))r.unshift(el);return r;}
function extLink(el){return links(el).find(a=>a.hostname&&a.hostname!==HOST);}
function norm(s){return s.toLowerCase().replace(/[^a-z0-9 ]/g,'').replace(/\s+/g,' ').trim();}
function isActionLink(a){
const t=txt(a).toLowerCase();
if(t.split(/\s+/).length<=3&&ACT_PAT.test(t))return true;
if(ACT_HREF.test(a.href))return true;
if(/\d+\s+(point|comment|vote|reply|reaction)/i.test(t))return true;
return false;
}
function bestLink(item,title){
const lnks=links(item).filter(a=>!isActionLink(a)&&a.offsetParent!==null);
if(lnks.length===0)return null;
const ext=lnks.find(a=>a.hostname&&a.hostname!==HOST);
if(ext)return ext;
if(title){
const tn=norm(title);
if(tn){const match=lnks.find(a=>{const an=norm(txt(a));return an&&tn.includes(an)&&an.length>3});if(match)return match;}
}
return lnks.reduce((best,a)=>{const at=txt(a).length,bt=txt(best).length;return at>bt?a:best;},lnks[0]);
}
// Shopping field extraction (universal e-commerce enhancement).
function rating(el){const a=el.querySelector('[aria-label*="star"],[aria-label*="rating"]');if(a){const al=a.getAttribute('aria-label')||'';const m=al.match(/(\d+\.?\d*)\s*out of\s*\d+/i)||al.match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const r=el.querySelector('[data-testid*="rating"],[class*="rating"],[class*="star"],[data-hook*="rating"],i[class*="a-icon-star"],span[class*="a-icon-alt"]');if(r){const m=(r.innerText||'').match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const t=txt(el);const tm=t.match(/(\d+\.?\d*)\s*(?:out of\s*5|\/5|stars?)/i);if(tm)return parseFloat(tm[1]);return null;}
function reviews(el){const t=txt(el);let m=t.match(/(\d[\d,]*)\s*(?:global\s+)?(?:ratings?|reviews?)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);m=t.match(/(\d[\d,]*)\s*(?:ratings?|reviews?)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);m=t.match(/(\d[\d,]*)\+?\s*(?:bought|purchased|sold)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);return null;}
function avail(el){const t=txt(el).toLowerCase();if(/in stock|in-store only/.test(t))return 'in stock';if(/out of stock|currently unavailable/.test(t))return 'out of stock';const ol=t.match(/only\s+(\d+)\s+left/i);if(ol)return 'only '+ol[1]+' left';const sh=t.match(/usually ships[^.]{0,50}/i);if(sh)return sh[0].trim();if(/pre-order/i.test(t))return 'pre-order';return null;}
function origPrice(el){const d=el.querySelector('del,s,[data-testid*="original"],[class*="was-price"],[class*="list-price"],[class*="original-price"]');if(d){const m=(d.innerText||'').match(PRICE);if(m)return m[0];}const t=txt(el);const m=t.match(/was\s+([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/i);if(m)return m[1];return null;}
function isSponsored(el){const t=txt(el).toLowerCase();if(t.includes('sponsored')||t.includes('sponsored ad'))return true;const b=el.querySelector('[data-testid*="sponsored"],[class*="sponsored"],[aria-label*="sponsored"]');return!!b;}
// Reddit field extraction (text-based, works through shadow DOM via innerText).
function rdScore(el){const t=txt(el);let m=t.match(/(\d[\d.,]*[KkMm]?)\s*(?:upvotes?|votes?|points?)/i);if(m)return parseCount(m[1]);return null;}
function rdComments(el){const t=txt(el);let m=t.match(/(\d[\d.,]*)\s*comments?/i);if(m)return parseInt(m[1].replace(/[,.]/g,''),10);return null;}
function rdAuthor(el){const a=el.querySelector('a[href*="/user/"]');if(a){const m=a.href.match(/\/user\/([\w-]+)/);if(m)return 'u/'+m[1];}const t=txt(el);const m=t.match(/u\/(\w[\w-]*)/i);if(m)return 'u/'+m[1];const img=el.querySelector('img[alt*="avatar"]');if(img){const m2=(img.alt||'').match(/u\/(\w[\w-]*)/i);if(m2)return 'u/'+m2[1];}return null;}
function rdSub(el){const u=location.href.match(/\/r\/(\w[\w-]*)/);if(u)return 'r/'+u[1];const t=txt(el);const m=t.match(/r\/(\w[\w-]*)/i);if(m)return 'r/'+m[1];return null;}
// GitHub field extraction.
function ghStars(el){var s=el.querySelector('a[href*="/stargazers"]');if(s){var m=(s.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}var sb=el.querySelector('button[aria-label*="star"],a[aria-label*="star"]');if(sb){var al=sb.getAttribute('aria-label')||'';var m2=al.match(/(\d[\d.,]*[KkMm]?)/);if(m2)return parseCount(m2[1]);}var t=txt(el);var m3=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\b/i);if(m3)return parseCount(m3[1]);return null;}
function ghForks(el){const f=el.querySelector('a[href*="/forks"]');if(f){const m=(f.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*forks?\b/i);if(m)return parseCount(m[1]);return null;}
function ghStarsToday(el){const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\s*today/i);if(m)return parseCount(m[1]);return null;}
function ghLabels(el){const ls=el.querySelectorAll('a[href*="label%3A"],a[data-name],.IssueLabel,.Label,.labels a');const r=[];for(const l of ls){const n=(l.getAttribute('data-name')||((l.innerText||'').split('\n')[0]||'')).trim();if(n&&n.length>1&&!r.includes(n))r.push(n);}return r;}
function ghNumber(el){const a=el.querySelector('a[data-testid="issue-pr-title-link"],a[href*="/issues/"],a[href*="/pull/"]');if(a){const m=(a.getAttribute('href')||'').match(/\/(?:issues|pull)\/(\d+)/);if(m)return parseInt(m[1],10);}const t=txt(el);const m=t.match(/#(\d+)/);if(m)return parseInt(m[1],10);return null;}
function ghStatus(el){const oc=el.querySelector('svg[class*="octicon-issue-open"]');if(oc)return 'open';const occ=el.querySelector('svg[class*="octicon-issue-closed"],svg[class*="octicon-skip"]');if(occ)return 'closed';const t=txt(el);const m=t.match(/Status:\s*(\w+)/i);if(m)return m[1].toLowerCase();if(el.querySelector('[data-testid="open-issue"],[class*="open-issue"],[aria-label*="open"]'))return 'open';if(el.querySelector('[data-testid="closed-issue"],[class*="closed-issue"],[aria-label*="closed"]'))return 'closed';return null;}
function isProductPage(){const b=document.body;if(!b)return false;const t=(b.innerText||'').toLowerCase();const hp=/[$€£¥₹]\s?\d/.test(t);const cb=document.querySelector('#add-to-cart-button,#buy-now-button,[data-testid*="add-to-cart"],[data-testid*="buy-now"],button[name*="cart"],input[name*="cart"],#add-to-cart,#buy-now');const hc=/add to cart|buy now|add to basket|add to bag|in winkelwagen|au panier/i.test(t);const u=location.href.toLowerCase();const pu=PROD_URL.test(u);const h1=document.querySelector('h1');const hh=h1&&h1.innerText.trim().length>5;return hp&&hh&&(pu||cb||hc);}
function offerBox(){const b=document.querySelector('#add-to-cart-button,#buy-now-button,[data-testid*="add-to-cart"],[data-testid*="buy-now"],button[name*="cart"],input[name*="cart"],#add-to-cart,#buy-now');if(!b)return null;let p=b;for(let i=0;i<7&&p&&p!==document.body;i++){p=p.parentElement;if(!p)break;const t=txt(p);if(t.length<4000&&PRICE.test(t))return p;}return b.closest('form,section,main,[role="main"]');}
function desc(el){if(!el||!el.tagName)return '';let s=el.tagName.toLowerCase();try{const id=el.getAttribute&&el.getAttribute('id');if(id&&id.length<=30)return s+'#'+id;const cl=(typeof el.className==='string'&&el.className.trim())?el.className.trim().split(/\s+/)[0]:'';if(cl&&cl.length<=24)s+='.'+cl;}catch(e){}return s;}
function selVariant(){const out=[];try{for(const op of document.querySelectorAll('option:checked')){const t=txt(op);if(t&&t.length<=120)out.push(t);}}catch(e){}try{for(const e of document.querySelectorAll('[aria-selected="true"],[aria-checked="true"],[data-selected="true"]')){if(e.tagName==='OPTION')continue;if(e.closest('nav,[role="tablist"],header'))continue;const t=txt(e);if(t&&t.length>=2&&t.length<=120)out.push(t);}}catch(e){}return out;}
function variantTable(needle){const nn=norm(needle);if(nn.length<2)return null;let tables=[];try{tables=[...document.querySelectorAll('table')].slice(0,5);}catch(e){return null;}for(const t of tables){const rows=[...t.querySelectorAll('tr')];if(rows.length<3)continue;const hdr=[...rows[0].querySelectorAll('th,td')].map(c=>norm(txt(c)));let j=-1,dup=false;for(let i=0;i<hdr.length;i++){if(hdr[i]&&(hdr[i].includes(nn)||nn.includes(hdr[i]))){if(j>=0){dup=true;break;}j=i;}}if(j<0||dup)continue;const specs=[];for(let r=1;r<rows.length&&specs.length<24;r++){const cs=[...rows[r].querySelectorAll('th,td')];if(cs.length<=j||!cs[j])continue;const k=txt(cs[0]).slice(0,80);const v=txt(cs[j]).slice(0,120);if(k)specs.push(k+': '+v);}if(specs.length>=2)return {column:txt(rows[0].querySelectorAll('th,td')[j]).slice(0,80),specs:specs};}return null;}
function sellerOf(box){if(!box)return null;const pats=['#sellerProfileTriggerId','[data-testid*="seller"]','[class*="seller"]','[id*="seller"]','[class*="merchant"]','[data-testid*="merchant"]'];for(const p of pats){const e=box.querySelector(p);if(e){const t=txt(e).slice(0,80);if(t&&t.length>1)return t;}}const m=txt(box).match(/(?:sold by|seller)\s*:?\s*([^.;\n]{2,60})/i);return m?m[1].trim():null;}
function deliveryOf(box){const b=box||document.body;const pats=['[data-testid*="delivery"]','[class*="delivery"]','[id*="delivery"]','[class*="shipping"]','[id*="shipping"]'];for(const p of pats){const e=b.querySelector(p);if(e){const t=txt(e).slice(0,120);if(t&&t.length>3)return t;}}const m=txt(b).match(/(deliver(?:y|s)? to[^.\n]{0,60}|ships? to[^.\n]{0,60}|cannot be shipped to[^.\n]{0,60}|does not ship to[^.\n]{0,60}|free (?:delivery|shipping)[^.\n]{0,40})/i);return m?m[1].trim():null;}
function extractProduct(){const o={};const notes=[];const h1=document.querySelector('h1');const title=h1?h1.innerText.trim():document.title;if(title)o.title=title.slice(0,300);o.url=location.href;
// Offer binding (G07): price first from the smallest ancestor of the buy
// button that carries one - a document-wide first match misattributes
// variant/related prices.
const box=offerBox();
const PRICE_SEL='#priceblock_ourprice,#priceblock_dealprice,.a-price .a-offscreen,[data-testid*="price"],[class*="price"]:not([class*="was"]):not([class*="original"]):not([class*="save"]),[id*="price"]:not([id*="was"]):not([id*="original"])';
let pe=box?box.querySelector(PRICE_SEL):null;if(!pe)pe=document.querySelector(PRICE_SEL);
if(pe){const m=(pe.innerText||'').match(PRICE);if(m){o.price=m[0];o.price_from=desc(pe);}}
if(!o.price){const dead=[...document.querySelectorAll('del,s,[class*="was-price"],[class*="original-price"],[class*="list-price"],[style*="line-through"]')].map(e=>(e.innerText||'').trim()).filter(Boolean);const ms=[...(document.body.innerText||'').matchAll(new RegExp(PRICE.source,'g'))].map(mm=>mm[1]);const pick=ms.find(pp=>!dead.some(dd=>dd.includes(pp)));if(pick){o.price=pick;o.price_from='page text';o.price_confidence='text-fallback';}else if(ms.length){notes.push('only crossed-out prices found on the page');}}
const op=origPrice(document.body);if(op)o.original_price=op;const rt=rating(document.body);if(rt!==null)o.rating=rt;const rv=reviews(document.body);if(rv!==null)o.reviews=rv;const av=avail(document.body);if(av)o.availability=av;const img=document.querySelector('#landingImage,#imgBlkFront,[data-testid*="product-image"],.product-image img,img[class*="product"]:not([src*="logo"]):not([src*="icon"]):not([src*="sprite"])');if(img&&img.src){o.image=img.src;if(img.alt)o.image_alt=img.alt.slice(0,100);}
const fs=[];const sec=document.querySelector('#feature-bullets,#productOverview_feature_div,#detailBullets_feature_div,[data-feature-name="productDescription"],#productDescription,#aplus,.product-facts-details,[data-testid="featureBullets"]')||(h1||pe||document.body).closest('section,div,main,[role="main"]')||document.body;if(sec){const bs=sec.querySelectorAll('li,[role="listitem"],span.a-list-item');for(const b of bs){const bt=(b.innerText||'').trim();if(bt.length>10&&bt.length<300&&fs.length<10&&!/add to cart|buy now|sign in|subscribe|follow|see more|show more/i.test(bt))fs.push(bt);}}if(fs.length>0)o.features=fs;
// Selected variant (single, unambiguous) + its spec-table column (G07):
// the benchmark defect - a default 500GB variant's speed attributed to the
// requested 2TB product. Multiple markers = ambiguity, reported as such.
const vuniq=[];for(const sv of selVariant()){const nv=norm(sv);if(nv&&nv.length>1&&!vuniq.some(u=>norm(u)===nv))vuniq.push(sv);}
if(vuniq.length===1){o.selected_variant=vuniq[0].slice(0,120);const vt=variantTable(vuniq[0]);if(vt){o.variant_column=vt.column;o.variant_specs=vt.specs;}}
else if(vuniq.length>1){o.variants_ambiguous=vuniq.slice(0,4).map(s2=>s2.slice(0,80));notes.push('multiple selected markers - price/spec binding to one variant is unverified');}
if(box){const se=sellerOf(box);if(se)o.seller=se;const dl=deliveryOf(box);if(dl)o.delivery=dl;o.offer_area=desc(box);}
if(notes.length)o.binding_note=notes.join('; ');
return o;}
function isRepoPage(){if(location.hostname!=='github.com')return false;const p=location.pathname.split('/').filter(Boolean);if(p.length<2)return false;if(['search','trending','explore','login','signup','settings','notifications','pulls','issues','orgs','features','marketplace','pricing','about','customer-stories','sessions','collections','topics','sponsors','new','dashboard','stars','pull','commit'].includes(p[0]))return false;if(p.length===2)return true;if(p.length>2&&['tree','blob'].includes(p[2]))return true;return false;}
function extractRepo(){const o={};const p=location.pathname.split('/').filter(Boolean);if(p.length>=2)o.title=p[0]+'/'+p[1];o.url=location.origin+'/'+p[0]+'/'+p[1];const og=document.querySelector('meta[property="og:description"]');const de=document.querySelector('p.f4,.BorderGrid-cell p,[itemprop="about"]');let dtx=og&&og.content?og.content.trim():(de?vtxt(de):'');if(dtx&&o.title&&dtx.endsWith(' - '+o.title))dtx=dtx.slice(0,dtx.length-o.title.length-3);if(dtx&&dtx.length>3)o.description=dtx.slice(0,300);const sc=document.querySelector('#repo-stars-counter-star');if(sc){const v=vtxt(sc).match(/[\d.,]+[KkMm]?/);if(v&&!isNaN(parseCount(v[0])))o.stars=parseCount(v[0]);}const fo=document.querySelector('#repo-network-counter');if(fo){const v=vtxt(fo).match(/[\d.,]+[KkMm]?/);if(v&&!isNaN(parseCount(v[0])))o.forks=parseCount(v[0]);}const lg=document.querySelector('[itemprop="programmingLanguage"]')||document.querySelector('a[href*="/search?l="]');if(lg){const t=vtxt(lg);if(t&&t.length<30)o.language=t;}const tp=[...document.querySelectorAll('a[href^="/topics/"]')].map(e=>vtxt(e)).filter(Boolean).slice(0,10);if(tp.length)o.topics=tp;return o;}
const SKIP_TAGS=new Set(['STYLE','SCRIPT','HEAD','NOSCRIPT','SVG','TEMPLATE','LINK','META','BR','HR','PATH','DEFS','USE','G','RECT','CIRCLE','LINE','POLYGON','POLYLINE']);
 // Site fast paths: known structures win over structural guessing. Reddit feeds
 // and comment pages carry data in element attributes; reading them directly is
 // exact and sidesteps hydration races where the generic detector fires before
 // the feed finishes streaming.
 if(IS_REDDIT){
 const isCp=location.pathname.indexOf('/comments/')>=0;
 // Post pages: hand off to the Rust comment sweep (reddit.rs) — it reads the
 // thread's own JSON endpoints and returns EVERY comment (collapsed replies
 // included) in one call. The DOM path below remains as the fallback.
 if(__POST_MARKER__&&isCp){var rbase=(location.pathname.match(/^(.*?\/comments\/[a-z0-9]+)/i)||[])[1];if(rbase){var rsort='confidence';var rsel=document.querySelector('[aria-selected=true],[aria-checked=true]');if(rsel){var rp=rsel;for(var ri=0;ri<6&&rp&&rp!==document.body;ri++){if(rp.tagName==='DATA'&&rp.getAttribute('value')){rsort=rp.getAttribute('value').toLowerCase();break;}rp=rp.parentElement;}}return JSON.stringify({container:'reddit-post-page',permalink:rbase,sort:rsort});}}
 // Search pages: fully client-rendered SDUI units (no `shreddit-post` ever
 // appears here), and the router swaps the results AFTER the url moves —
 // reading the mounted DOM is exactly the stale window. Hand off to the Rust
 // listing sweep, which reads reddit's own JSON (exact scores, query echo).
 if(__POST_MARKER__){var spath=location.pathname;if(/\/search\/?$/.test(spath)){var sqs=new URLSearchParams(location.search);var sty=sqs.get('type')||'';if((sty===''||sty==='posts'||sty==='links'||sty==='all')&&(sqs.get('q')||'').trim()){return JSON.stringify({container:'reddit-search-page',path:spath,params:location.search});}}}
 const cmts=[...document.querySelectorAll('shreddit-comment')];
 const posts=[...document.querySelectorAll('shreddit-post')];
 if(isCp&&cmts.length>=3){
 const items=cmts.slice(0,__LIMIT__).map(sc=>{const o={};const a=n=>sc.getAttribute(n)||'';const au=a('author');if(au)o.author='u/'+au;const s=a('score');if(s!=='')o.fuzzed_score=parseInt(s,10);const d=a('depth');if(d!=='')o.depth=parseInt(d,10);const pl=a('permalink');if(pl)o.url='https://www.reddit.com'+(pl.charAt(0)==='/'?pl:'/'+pl);const cr=a('created');if(cr)o.date=cr.slice(0,16).replace('T',' ');const bd=sc.querySelector('[slot="comment"]');const bt=bd?vtxt(bd):'';if(bt)o.text=bt.slice(0,500);return o;}).filter(o=>Object.keys(o).length>0);
 if(items.length>0)return JSON.stringify({container:'reddit-comments',count:items.length,items,note:"fuzzed_score values are Reddit's displayed (fuzzed) scores"});
 }
 if(!isCp&&posts.length>=3){
 const items=posts.slice(0,__LIMIT__).map(sp=>{const o={};const a=n=>sp.getAttribute(n)||'';const pt=a('post-title');if(pt)o.title=pt.slice(0,200);const pl=a('permalink');if(pl)o.url='https://www.reddit.com'+(pl.charAt(0)==='/'?pl:'/'+pl);const s=a('score');if(s!=='')o.fuzzed_score=parseInt(s,10);const cc=a('comment-count');if(cc!=='')o.comments=parseInt(cc,10);const au=a('author');if(au)o.author='u/'+au;const sub=a('subreddit-prefixed-name')||a('subreddit-name');if(sub)o.subreddit=sub.indexOf('/')>=0?sub:'r/'+sub;const ts=a('created-timestamp');if(ts)o.date=ts.slice(0,16).replace('T',' ');const dm=a('domain');if(dm)o.domain=dm;const ty=a('post-type');if(ty)o.type=ty;const ch=a('content-href');if(ch)o.content_href=ch.slice(0,300);return o;}).filter(o=>Object.keys(o).length>0);
 if(items.length>0)return JSON.stringify({container:'reddit-feed',count:items.length,items,note:"fuzzed_score values are Reddit's displayed (fuzzed) scores"});
 }
 }
 // Hacker News: server-rendered rows — the subtext line carries points,
 // author, age and the comment count; the generic path dropped them all.
 const IS_HN=HOST==='news.ycombinator.com';
 if(IS_HN){
 const rows=[...document.querySelectorAll('tr.athing')];
 if(rows.length>=3){
 const items=rows.slice(0,__LIMIT__).map(tr=>{const o={};
 const ta=tr.querySelector('span.titleline>a')||tr.querySelector('a.titlelink');
 if(ta){const t=vtxt(ta);if(t)o.title=t.slice(0,200);const h=ta.getAttribute('href')||'';if(h)o.url=/^https?:/.test(h)?h:location.origin+'/'+(h.charAt(0)==='/'?h.slice(1):h);}
 const st=tr.nextElementSibling?tr.nextElementSibling.querySelector('.subtext'):null;
 if(st){const sc=st.querySelector('.score');if(sc){const m=vtxt(sc).replace(/\u00a0/g,' ').match(/(\d[\d,]*)\s*point/i);if(m)o.points=parseInt(m[1].replace(/,/g,''),10);}
 const au=st.querySelector('a.hnuser');if(au){const a=vtxt(au);if(a)o.author=a;}
 const ag=st.querySelector('span.age a');if(ag){const a=vtxt(ag);if(a)o.age=a;}
 const cl=[...st.querySelectorAll('a')].find(a=>/comment|discuss/i.test(vtxt(a)));
 if(cl){const m=vtxt(cl).replace(/\u00a0/g,' ').match(/(\d[\d,]*)/);o.comments=m?parseInt(m[1].replace(/,/g,''),10):0;}}
 return o;}).filter(o=>o.title);
 if(items.length>0)return JSON.stringify({container:'hn-items',count:items.length,items});
 }
 }
 const IS_X=HOST==='x.com'||HOST==='twitter.com';
 // X: hand off to the Rust graphql fast path (x.rs) — the virtualized DOM
 // only holds a few mounted cells; the page's own API traffic is complete.
 if(__POST_MARKER__&&IS_X){
 const p=location.pathname.split('/').filter(Boolean);
 let xkind='other';
 if(p.indexOf('status')>=0)xkind='status';
 else if(location.pathname.startsWith('/search'))xkind='search';
 else if(location.pathname==='/home')xkind='home';
 else if(p.length===1)xkind='profile';
 return JSON.stringify({container:'x-page',kind:xkind});
 }
 if(IS_GITHUB){
 const tl=[...document.querySelectorAll('a[data-testid="issue-pr-title-link"]')];
 if(tl.length>=3){
 const items=tl.slice(0,__LIMIT__).map(a=>{const o={};const t=vtxt(a);if(t)o.title=t.slice(0,200);const h=a.getAttribute('href')||'';if(h)o.url='https://github.com'+h;const m=h.match(/\/(?:issues|pull)\/(\d+)/);if(m)o.number=parseInt(m[1],10);const row=a.closest('li')||a.closest('div');if(row){const lb=[...row.querySelectorAll('a[href*="label%3A"]')].map(e=>(e.innerText||'').split('\n')[0].trim()).filter(n=>n&&n.length>1);if(lb.length)o.labels=lb;const us=row.querySelector('a[data-hovercard-type="user"]');const ut=us?vtxt(us):'';if(ut)o.author=ut;const so=row.querySelector('svg[class*="octicon-issue-open"]');if(so)o.status='open';else{const sx=row.querySelector('svg[class*="octicon-issue-closed"],svg[class*="octicon-skip"]');if(sx)o.status='closed';else{const spr=row.querySelector('svg[class*="octicon-git-pull-request"],svg[class*="octicon-git-merge"]');if(spr)o.status='pr';}}}return o;}).filter(o=>o.title||o.url);
 if(items.length>0)return JSON.stringify({container:'github-issues',count:items.length,items});
 }
 }
 // Repo root: the repo itself is the payload (not the commit feed).
 if(IS_GITHUB&&isRepoPage()&&location.pathname.split('/').filter(Boolean).length===2){const rr=extractRepo();if(Object.keys(rr).length>1)return JSON.stringify({container:'github-repo',count:1,items:[rr]});}
 // Product detail page (URL says so): the product is the payload, not its
 // feature-bullet list. Listing pages (no product URL) keep the list path.
 if(PROD_URL.test(location.href.toLowerCase())&&isProductPage()){const pp=extractProduct();if(Object.keys(pp).length>1)return JSON.stringify({container:'product',count:1,items:[pp]});}
 let best=null,bestScore=0,bestSig='';
for(const c of document.querySelectorAll('*')){
if(SKIP_TAGS.has(c.tagName))continue;
const kids=[...c.children].filter(k=>k.nodeType===1&&k.offsetParent!==null);
if(kids.length<3)continue;
const groups={};
for(const k of kids){const s=sig(k);if(!groups[s])groups[s]=[];groups[s].push(k);}
for(const s in groups){
let items=groups[s];
if(items.length<3)continue;
if(items[0]&&SKIP_TAGS.has(items[0].tagName))continue;
// Quality gate: a real list item is VISIBLE — it has rendered text, a link,
// or an image. Script bundles and hidden containers have textContent only;
// they used to win with raw JS source as the "title".
// Quality floor: a real list item carries substance — speaking-length
// text, a link, an image, or a price. Unit-count fragments like
// "2 units" used to win as item "titles" on listing sites.
const isItem=it=>{const t=vtxt(it);return t.length>=12||it.querySelector('img[src]')||links(it).length>0||PRICE.test(t);};
items=items.filter(isItem);
if(items.length<3)continue;
let totalText=0,extCount=0,hCount=0,imgCount=0,linkCount=0;
for(const it of items){totalText+=vtxt(it).length;if(extLink(it))extCount++;if(it.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]'))hCount++;if(it.querySelector('img[src]'))imgCount++;if(links(it).length>0)linkCount++;}
const count=items.length;const avgText=totalText/count;
if(avgText<12)continue;
const tf=Math.min(Math.max(avgText/50,0.5),4);
const score=count*tf*(1+(extCount/count)*2+(hCount/count)+(imgCount/count)*0.5+(linkCount/count)*0.3);
if(score>bestScore){bestScore=score;best=c;bestSig=s;}
}
}
if(!best){if(IS_GITHUB&&isRepoPage()){const r=extractRepo();if(Object.keys(r).length>1)return JSON.stringify({container:'github-repo',count:1,items:[r]});}return JSON.stringify({error:'no repeated list found',items:[]});}
// Field extraction: clean, typed, deduplicated. No 'text' field.
const items=[...best.children].filter(k=>k.nodeType===1&&sig(k)===bestSig).map(item=>{
if(item.tagName==='SHREDDIT-AD-POST'||item.querySelector('shreddit-ad-post'))return {};
const fullText=vtxt(item);const o={};
// Title: heading → longest link text → first sentence.
const h=item.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]');
let title=h?vtxt(h):'';
const link=bestLink(item,title);
if(link){const lt=vtxt(link);if(lt&&(!title||title.length<10||lt.length>title.length*1.5))title=lt.slice(0,200);}
if(!title){const sentence=fullText.split(/\.|!|\?/)[0];title=(sentence&&sentence.length>10?sentence:fullText).slice(0,200);}
// Numeric/unit-like fragments ("2 units", "3 beds") are not titles.
if(title&&/^\s*[\d.,]+\s*(units?|beds?|baths?|ba|bd|mi|miles?|sq\.?\s?ft|sqft|acres?)?\s*$/i.test(title)){const lt2=fullText.replace(/\s+/g,' ').trim();if(lt2.length>title.length+6)title=lt2.slice(0,200);}
// A price glued into the title (listing-card text) is a field, not a name.
if(title){const pi=title.search(PRICE);if(pi>=10)title=title.slice(0,pi).trim();}
if(title)o.title=title.slice(0,200);
if(link)o.url=link.href;
// Image.
const img=item.querySelector('img[src]');
if(img){o.image=img.src;if(img.alt)o.image_alt=img.alt.slice(0,100);}
// Price: currency symbol required. Emitted even when the title carries it —
// structured fields are the point (listing cards glue address+price).
const pr=(fullText.match(PRICE)||[])[0];
if(pr)o.price=pr;
// Date: only in non-title text.
const nonTitle=fullText.slice((title||'').length);
const dt=(nonTitle.match(DATE)||[])[0];
if(dt)o.date=dt;
// Description: non-link, non-heading text. Only if different from title.
const clone=item.cloneNode(true);
clone.querySelectorAll('a,script,style,noscript,svg').forEach(e=>e.remove());
const hd=clone.querySelector('h1,h2,h3,h4,h5,h6');
if(hd)hd.remove();
const desc=(clone.innerText||'').replace(/\s+/g,' ').trim();
if(desc&&desc.length>15){const dn=norm(desc),tn=norm(title||'');if(dn&&!tn.includes(dn)&&!dn.includes(tn))o.description=desc.slice(0,300);}
// Site-specific fields.
if(IS_REDDIT){var sp=item.tagName==='SHREDDIT-POST'?item:item.querySelector('shreddit-post');var sc0=item.tagName==='SHREDDIT-COMMENT'?item:item.querySelector('shreddit-comment');if(sp){var sps=sp.getAttribute('score');if(sps)o.fuzzed_score=parseInt(sps,10);var spc=sp.getAttribute('comment-count');if(spc)o.comments=parseInt(spc,10);var spa=sp.getAttribute('author');if(spa)o.author='u/'+spa;var spsub=sp.getAttribute('subreddit-prefixed-name');if(spsub)o.subreddit=spsub;var spt=sp.getAttribute('post-title');if(spt)o.title=spt.slice(0,200);var spl=sp.getAttribute('permalink');if(spl)o.url='https://www.reddit.com'+(spl.charAt(0)==='/'?spl:'/'+spl);var spts=sp.getAttribute('created-timestamp');if(spts)o.date=spts.slice(0,16).replace('T',' ');}else if(sc0){var sca=sc0.getAttribute('author');if(sca)o.author='u/'+sca;var scs=sc0.getAttribute('score');if(scs!=='')o.fuzzed_score=parseInt(scs,10);var scd=sc0.getAttribute('depth');if(scd!=='')o.depth=parseInt(scd,10);var scb=sc0.querySelector('[slot="comment"]');var sct=scb?vtxt(scb).slice(0,400):'';if(sct)o.text=sct;}else if(item.classList&&item.classList.contains('thing')){var g2=n=>item.getAttribute(n)||'';var gs2=g2('data-score');if(gs2)o.score=parseInt(gs2,10);var gc2=g2('data-comments-count');if(gc2)o.comments=parseInt(gc2,10);var ga2=g2('data-author');if(ga2)o.author='u/'+ga2;var gsb2=g2('data-subreddit');if(gsb2)o.subreddit='r/'+gsb2;var gp2=g2('data-permalink');if(gp2)o.url='https://www.reddit.com'+gp2;var gtl=item.querySelector('a.title');if(gtl)o.title=vtxt(gtl).slice(0,200);}else{var rsc=rdScore(item);if(rsc!==null)o.fuzzed_score=rsc;var rcm=rdComments(item);if(rcm!==null)o.comments=rcm;var rau=rdAuthor(item);if(rau)o.author=rau;var rsu=rdSub(item);if(rsu)o.subreddit=rsu;var rcl=item.querySelector('a[href*="/comments/"]');if(rcl){o.url=rcl.href;var rclt=vtxt(rcl);if(rclt&&rclt.length>5&&rclt.length<300)o.title=rclt.slice(0,200);}}}
else if(IS_GITHUB){const st=ghStars(item);if(st!==null)o.stars=st;const fk=ghForks(item);if(fk!==null)o.forks=fk;const sd=ghStarsToday(item);if(sd!==null)o.stars_today=sd;const lb=ghLabels(item);if(lb.length>0)o.labels=lb;const nm=ghNumber(item);if(nm!==null)o.number=nm;const gs=ghStatus(item);if(gs)o.status=gs;}
else{const rt=rating(item);if(rt!==null)o.rating=rt;const rv=reviews(item);if(rv!==null)o.reviews=rv;const av=avail(item);if(av)o.availability=av;const op=origPrice(item);if(op)o.original_price=op;if(isSponsored(item))o.sponsored=true;}
return o;
}).filter(o=>Object.keys(o).length>0).slice(0,__LIMIT__);
const lowConf=items.length>0&&items.every(o=>!o.url)&&(items.reduce((a,o)=>a+(o.title||'').length,0)/items.length)<25;
return JSON.stringify(lowConf?{container:best.tagName.toLowerCase(),count:items.length,items,confidence:'low',note:'no clear list found — items may be page fragments; verify before relying on them'}:{container:best.tagName.toLowerCase(),count:items.length,items});
})()