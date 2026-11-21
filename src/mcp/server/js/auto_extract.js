(()=>{
// Price: currency symbol required (no bare decimal numbers — false positives).
const PRICE=/([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/;
const PROD_URL=/\/dp\/|\/gp\/product\/|\/product\/|\/itm\/|\/products\/|\/p\//;
const DATE=/(\b\d{4}-\d{2}-\d{2}\b|\b\d{1,2}[\/]\d{1,2}[\/]\d{2,4}\b|\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+\d{1,2},?\s*\d{2,4}\b|\b\d+\s+(?:second|minute|hour|day|week|month|year)s?\s+ago\b)/i;
const HOST=(location.hostname||'').toLowerCase();
function hostMatches(domain){return HOST===domain||HOST.endsWith('.'+domain);}
const IS_REDDIT=hostMatches('reddit.com');
const IS_GITHUB=HOST==='github.com';
const IS_HN=HOST==='news.ycombinator.com';
const IS_X=hostMatches('x.com')||hostMatches('twitter.com');
function parseCount(s){if(s==null)return null;s=String(s).trim();if(!s)return null;const m=s.match(/^(-?\d[\d,]*(?:\.\d+)?)\s*([KkMmBb])?$/);let n,suf='';if(m){n=parseFloat(m[1].replace(/,/g,''));suf=(m[2]||'').toLowerCase();}else{n=parseFloat(s.replace(/,/g,''));if(/b$/i.test(s))suf='b';else if(/m$/i.test(s))suf='m';else if(/k$/i.test(s))suf='k';}if(!isFinite(n))return null;if(suf==='k')n*=1000;else if(suf==='m')n*=1000000;else if(suf==='b')n*=1000000000;return Math.round(n);}
const ACT_PAT=/^(vote|upvote|downvote|comment|comments|discuss|reply|replies|share|save|hide|flag|report|favorite|fav|like|dislike|follow|Subscribe|Pin|Unpin|More|less|edit|delete|remove|add|new|open|show|expand|collapse|permalink|embed|cite|parent|context|full story|read more|continue reading|view|all|next|prev|previous)$/i;
const ACT_HREF=/\/vote|\/comment|\/reply|\/action|javascript:|#comment|#respond|#reply/i;
const _vtxtCache=new WeakMap();
function sig(el){const k=[...el.children].map(c=>c.tagName).join(',');return el.tagName+'['+k+']';}
function txt(el){if(!el)return '';return(el.innerText||'').replace(/\s+/g,' ').trim();}
function vtxt(el){if(!el)return '';if(typeof el==='object'){const c=_vtxtCache.get(el);if(c!==undefined)return c;}const v=(el.innerText||'').replace(/\s+/g,' ').trim();if(typeof el==='object')_vtxtCache.set(el,v);return v;}
function tcnt(el){if(!el)return '';return(el.textContent||'').replace(/\s+/g,' ').trim();}
function isShown(el){if(!el)return false;if(el.offsetParent!==null&&el.offsetParent!==undefined)return true;if(typeof el.getClientRects==='function')return el.getClientRects().length>0;return false;}
function absURL(href){if(!href)return '';const h=String(href).trim();if(!h||h.startsWith('#')||/^javascript:/i.test(h))return '';try{return new URL(h,location.href).href;}catch(e){return '';}}
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
// SVG <a> also matches a[href], but its .href is an SVGAnimatedString and
// its offsetParent is undefined: it passed the visibility filter and became
// the item url "/[object SVGAnimatedString]".
const lnks=links(item).filter(a=>typeof a.href==='string'&&!isActionLink(a)&&isShown(a));
if(lnks.length===0)return null;
const ext=lnks.find(a=>a.hostname&&a.hostname!==HOST);
if(ext)return ext;
if(title){
const tn=norm(title);
if(tn){const match=lnks.find(a=>{const an=norm(txt(a));return an&&tn.includes(an)&&an.length>3});if(match)return match;}
}
return lnks.reduce((best,a)=>{const at=txt(a).length,bt=txt(best).length;return at>bt?a:best;},lnks[0]);
}
// Redirect/tracking wrappers (Amazon /sspa/click?url=, Google /url?q=,
// Bing /ck/a?u=a1<base64url>, DuckDuckGo /l/?uddg=, eBay /clk, ad /aclk, ...):
// the real destination is recoverable, and an agent can do nothing with a
// wrapper URL — it is an interstitial, not the item. Unwrap up to 2 hops;
// shapes we don't recognise pass through intact.
const REDIRECT_PATH=/\/(?:sspa|gp)\/click|\/ck\/a\/?$|\/l\/?$|\/aclk\/?$|\/clk\/?$|\/url\/?$|\/redirect(?:or)?\/?$|\/linkout|\/l\.php$|\/away\/?$|\/out\/?$|\/goto\/?$|\/go\/?$|servlet\/redirect/i;
const REDIRECT_PARAMS=['url','u','q','uddg','target','to','redirect','redirect_uri','link','dest','destination','href'];
function decodeBingU(v){if(!v||!/^a1[A-Za-z0-9_-]{8,}$/.test(v))return null;try{const b64=v.slice(2).replace(/-/g,'+').replace(/_/g,'/');const pad=b64+'='.repeat((4-b64.length%4)%4);const decoded=typeof atob==='function'?atob(pad):Buffer.from(pad,'base64').toString('utf8');return /^https?:\/\//i.test(decoded)?decoded:null;}catch(e){return null;}}
function canonicalURL(href){try{let u=new URL(href,location.href);for(let i=0;i<2&&REDIRECT_PATH.test(u.pathname);i++){let next=null;if(/\/ck\/a\/?$/i.test(u.pathname)){const bu=decodeBingU(u.searchParams.get('u'));if(bu){next=bu;}}if(!next){for(const p of REDIRECT_PARAMS){const v=u.searchParams.get(p);if(!v)continue;let d=v;if(!/^https?:\/\//i.test(d)&&d.charAt(0)!=='/'){try{d=decodeURIComponent(v);}catch(e){}}if(/^https?:\/\//i.test(d)){next=d;break;}if(d.charAt(0)==='/'){next=new URL(d,u).href;break;}}}if(!next)break;try{u=new URL(next);}catch(e){break;}}return u.href;}catch(e){return href;}}
// Real articles can end in /URL too; a path alone is not a redirect.
function wrappedURL(href){try{const u=new URL(href,location.href);return /^https?:$/.test(u.protocol)&&REDIRECT_PATH.test(u.pathname)&&REDIRECT_PARAMS.some(p=>!!u.searchParams.get(p));}catch(e){return false;}}
// How many results the page TEXT claims vs. how many are in the DOM. This is
// the lazy-load/pagination tell: a page that says 3,412 with 24 items loaded
// answers a "show me the results" question with a fraction of them. Advisory
// only — it never invents items, it reports what the page itself states.
function pageClaim(){const t=(document.body&&document.body.innerText?document.body.innerText:'').slice(0,60000);
const pats=[[/showing\s+[\d.,]+\s*[-\u2013\u2014]\s*[\d.,]+\s+of\s+([\d.,]+)/i,true],[/[\d.,]+\s+of\s+([\d.,]+)\s+(?:results?|items?|listings|products|entries|matches|hits)/i,true],[/(?:about|over|more than)\s+([\d.,]+)\s+(?:results?|items?|listings|products|entries|matches|hits)/i,false],[/([\d.,]+)\s+(?:results?|listings|products|entries|matches|hits)\b/i,false]];
for(const pp of pats){const m=t.match(pp[0]);if(m){const v=parseInt(m[1].replace(/[.,]/g,''),10);if(v>=20&&v<1e9)return {total:v,paged:pp[1]};}}
return null;}
// Shopping field extraction (universal e-commerce enhancement).
function rating(el){const a=el.querySelector('[aria-label*="star"],[aria-label*="rating"]');if(a){const al=a.getAttribute('aria-label')||'';const m=al.match(/(\d+\.?\d*)\s*out of\s*\d+/i)||al.match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const r=el.querySelector('[data-testid*="rating"],[class*="rating"],[class*="star"],[data-hook*="rating"],i[class*="a-icon-star"],span[class*="a-icon-alt"]');if(r){const m=(r.innerText||r.textContent||'').match(/(\d+\.?\d*)/);if(m)return parseFloat(m[1]);}const t=txt(el);const tm=t.match(/(\d+\.?\d*)\s*(?:out of\s*5|\/5|stars?)/i);if(tm)return parseFloat(tm[1]);return null;}
function reviews(el){const t=txt(el);let m=t.match(/(\d[\d,]*)\s*(?:global\s+)?(?:ratings?|reviews?)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);m=t.match(/(\d[\d,]*)\+?\s*(?:bought|purchased|sold)/i);if(m)return parseInt(m[1].replace(/,/g,''),10);return null;}
function avail(el){const t=txt(el).toLowerCase();if(/in stock|in-store only/.test(t))return 'in stock';if(/out of stock|currently unavailable/.test(t))return 'out of stock';const ol=t.match(/only\s+(\d+)\s+left/i);if(ol)return 'only '+ol[1]+' left';const sh=t.match(/usually ships[^.]{0,50}/i);if(sh)return sh[0].trim();if(/pre-order/i.test(t))return 'pre-order';return null;}
function origPrice(el){const d=el.querySelector('del,s,[data-testid*="original"],[class*="was-price"],[class*="list-price"],[class*="original-price"]');if(d){const m=(d.innerText||d.textContent||'').match(PRICE);if(m)return m[0];}const t=txt(el);const m=t.match(/was\s+([$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?)/i);if(m)return m[1];return null;}
function isSponsored(el){const t=txt(el).toLowerCase();if(t.includes('sponsored')||t.includes('sponsored ad'))return true;const b=el.querySelector('[data-testid*="sponsored"],[class*="sponsored"],[aria-label*="sponsored"]');return!!b;}
// Reddit field extraction (text-based fallback, works through shadow DOM via innerText).
function rdScore(el){const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*(?:upvotes?|votes?|points?)/i);return m?parseCount(m[1]):null;}
function rdComments(el){const t=txt(el);const m=t.match(/(\d[\d.,]*)\s*comments?/i);return m?parseInt(m[1].replace(/[,.]/g,''),10):null;}
function rdAuthor(el){const a=el.querySelector('a[href*="/user/"]');if(a){const m=a.href.match(/\/user\/([\w-]+)/);if(m)return 'u/'+m[1];}const t=txt(el);const m=t.match(/u\/(\w[\w-]*)/i);if(m)return 'u/'+m[1];const img=el.querySelector('img[alt*="avatar"]');if(img){const m2=(img.alt||'').match(/u\/(\w[\w-]*)/i);if(m2)return 'u/'+m2[1];}return null;}
function rdSub(el){const u=location.href.match(/\/r\/(\w[\w-]*)/);if(u)return 'r/'+u[1];const t=txt(el);const m=t.match(/r\/(\w[\w-]*)/i);if(m)return 'r/'+m[1];return null;}
// GitHub field extraction.
function ghStars(el){const s=el.querySelector('a[href*="/stargazers"]');if(s){const m=(s.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}const sb=el.querySelector('button[aria-label*="star"],a[aria-label*="star"]');if(sb){const al=sb.getAttribute('aria-label')||'';const m2=al.match(/(\d[\d.,]*[KkMm]?)/);if(m2)return parseCount(m2[1]);}const t=txt(el);const m3=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\b/i);return m3?parseCount(m3[1]):null;}
function ghForks(el){const f=el.querySelector('a[href*="/forks"]');if(f){const m=(f.innerText||'').match(/(\d[\d.,]*[KkMm]?)/);if(m)return parseCount(m[1]);}const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*forks?\b/i);return m?parseCount(m[1]):null;}
function ghStarsToday(el){const t=txt(el);const m=t.match(/(\d[\d.,]*[KkMm]?)\s*stars?\s*today/i);return m?parseCount(m[1]):null;}
function ghLabels(el){const ls=el.querySelectorAll('a[href*="label%3A"],a[data-name],.IssueLabel,.Label,.labels a');const r=[];for(const l of ls){const n=(l.getAttribute('data-name')||((l.innerText||'').split('\n')[0]||'')).trim();if(n&&n.length>1&&!r.includes(n))r.push(n);}return r;}
function ghNumber(el){const a=el.querySelector('a[data-testid="issue-pr-title-link"],a[data-testid="listitem-title-link"],a[href*="/issues/"],a[href*="/pull/"]');if(a){const m=(a.getAttribute('href')||'').match(/\/(?:issues|pull)\/(\d+)/);if(m)return parseInt(m[1],10);}const t=txt(el);const m=t.match(/#(\d+)/);return m?parseInt(m[1],10):null;}
function ghStatus(el){const oc=el.querySelector('svg[class*="octicon-issue-open"]');if(oc)return 'open';const occ=el.querySelector('svg[class*="octicon-issue-closed"],svg[class*="octicon-skip"]');if(occ)return 'closed';const t=txt(el);const m=t.match(/Status:\s*(\w+)/i);if(m)return m[1].toLowerCase();if(el.querySelector('[data-testid="open-issue"],[class*="open-issue"],[aria-label*="open"]'))return 'open';if(el.querySelector('[data-testid="closed-issue"],[class*="closed-issue"],[aria-label*="closed"]'))return 'closed';return null;}
// A price + h1 under a /p/-style URL is also every newsletter post that
// mentions "$5" (Substack serves every post at /p/<slug>): the URL never
// makes a product page on its own; a cart/buy control or cart wording must.
function isProductPage(){const b=document.body;if(!b)return false;const t=(b.innerText||'').toLowerCase();const hp=/[$€£¥₹]\s?\d/.test(t);const cb=document.querySelector('#add-to-cart-button,#buy-now-button,[data-testid*="add-to-cart"],[data-testid*="buy-now"],button[name*="cart"],input[name*="cart"],#add-to-cart,#buy-now');const cm=t.match(/add to cart|buy now|add to basket|add to bag|in winkelwagen|au panier/gi);if(cm&&cm.length>=3)return false;const hc=!!(cm&&cm.length>0);const h1=document.querySelector('h1');const hh=h1&&h1.innerText.trim().length>5;return hp&&hh&&(cb||hc);}
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
const PRICE_SEL='#priceblock_ourprice,#priceblock_dealprice,.a-price:not(.a-text-price) .a-offscreen,[data-testid*="price"],[class*="price"]:not([class*="was"]):not([class*="original"]):not([class*="save"]),[id*="price"]:not([id*="was"]):not([id*="original"])';
let pe=box?box.querySelector(PRICE_SEL):null;if(!pe)pe=document.querySelector(PRICE_SEL);
if(pe){const m=(pe.innerText||pe.textContent||'').match(PRICE);if(m){o.price=m[0];o.price_from=desc(pe);}}
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
function extractRepo(){const o={};const p=location.pathname.split('/').filter(Boolean);if(p.length>=2)o.title=p[0]+'/'+p[1];o.url=location.origin+'/'+p[0]+'/'+p[1];const og=document.querySelector('meta[property="og:description"]');const de=document.querySelector('p.f4,.BorderGrid-cell p,[itemprop="about"]');let dtx=og&&og.content?og.content.trim():(de?vtxt(de):'');if(dtx&&o.title&&dtx.endsWith(' - '+o.title))dtx=dtx.slice(0,dtx.length-o.title.length-3);if(dtx&&dtx.length>3)o.description=dtx.slice(0,300);const sc=document.querySelector('#repo-stars-counter-star,#repo-stars-counter-unstar')||document.querySelector('a[href$="/stargazers"] strong,a[href*="/stargazers"]');if(sc){const v=(sc.getAttribute('title')||vtxt(sc)).match(/[\d.,]+[KkMm]?/);const n=v?parseCount(v[0]):null;if(n!==null)o.stars=n;}if(o.stars===undefined){const aside=document.querySelector('.Layout-sidebar,.BorderGrid')||document.body;const sm=aside?vtxt(aside).match(/(\d[\d.,]*[KkMm]?)\s+stars?\b/i):null;const n=sm?parseCount(sm[1]):null;if(n!==null)o.stars=n;}const fo=document.querySelector('#repo-network-counter')||document.querySelector('a[href$="/forks"] strong,a[href*="/forks"]');if(fo){const v=(fo.getAttribute('title')||vtxt(fo)).match(/[\d.,]+[KkMm]?/);const n=v?parseCount(v[0]):null;if(n!==null)o.forks=n;}const lg=document.querySelector('[itemprop="programmingLanguage"]')||document.querySelector('a[href*="/search?l="]');if(lg){const t=vtxt(lg).replace(/\s+\d+(?:\.\d+)?%\s*$/,'').trim();if(t&&t.length<30)o.language=t;}const lic=document.querySelector('a[href$="/blob/main/LICENSE"],a[href$="/blob/master/LICENSE"],a[data-analytics-event*="license"]');if(lic&&vtxt(lic)&&vtxt(lic).length<60)o.license=vtxt(lic).replace(/\s*license$/i,'').trim();const rel=document.querySelector('a[href*="/releases/tag/"] span.css-truncate-target, a[href*="/releases/tag/"] .text-bold');if(rel&&vtxt(rel)&&vtxt(rel).length<60)o.latest_release=vtxt(rel);const tp=[...new Set([...document.querySelectorAll('a[href^="/topics/"]')].map(e=>vtxt(e)).filter(Boolean))].slice(0,10);if(tp.length)o.topics=tp;return o;}
const SKIP_TAGS=new Set(['STYLE','SCRIPT','HEAD','NOSCRIPT','SVG','TEMPLATE','LINK','META','BR','HR','PATH','DEFS','USE','G','RECT','CIRCLE','LINE','POLYGON','POLYLINE']);
const SEMANTIC_LIST_CONTAINERS=new Set(['UL','OL','TBODY']);
function groupKey(parent,child){
const pRole=parent.getAttribute&&parent.getAttribute('role');
const cRole=child.getAttribute&&child.getAttribute('role');
const dt=child.getAttribute&&(child.getAttribute('data-testid')||child.getAttribute('data-component-type'));
if(dt)return child.tagName+'@'+dt;
if(cRole==='listitem'||cRole==='article'||child.tagName==='ARTICLE')return child.tagName+':'+(cRole||'article');
if(SEMANTIC_LIST_CONTAINERS.has(parent.tagName)||pRole==='list'||pRole==='feed')return child.tagName+'*';
return sig(child);
}
// Site fast paths: modular adapters assembled from src/adapters/js/*.js.
__SITE_ADAPTERS__
// Product detail page (URL says so): the product is the payload, not its
// feature-bullet list. Listing pages (no product URL) keep the list path.
if(PROD_URL.test(location.href.toLowerCase())&&isProductPage()){const pp=extractProduct();if(Object.keys(pp).length>1)return JSON.stringify({container:'product',detail:true,count:1,items:[pp]});}
// Quality gate: a real list item is VISIBLE and carries substance — speaking-
// length text, a link, an image, or a price. Script bundles, hidden templates,
// and unit-count fragments like "2 units" are excluded from both scoring AND
// extraction so hidden duplicates never leak into results.
const isItem=it=>{const t=vtxt(it);return t.length>=12||it.querySelector('img[src]')||links(it).length>0||PRICE.test(t);};
let best=null,bestScore=0,bestKey='';
for(const c of document.querySelectorAll('*')){
if(SKIP_TAGS.has(c.tagName))continue;
const kids=[...c.children].filter(k=>k.nodeType===1&&isShown(k));
if(kids.length<3)continue;
const groups={};
for(const k of kids){const s=groupKey(c,k);if(!groups[s])groups[s]=[];groups[s].push(k);}
for(const s in groups){
let items=groups[s];
if(items.length<3)continue;
if(items[0]&&SKIP_TAGS.has(items[0].tagName))continue;
items=items.filter(isItem);
if(items.length<3)continue;
let totalText=0,extCount=0,hCount=0,imgCount=0,linkCount=0;
for(const it of items){totalText+=vtxt(it).length;if(extLink(it))extCount++;if(it.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]'))hCount++;if(it.querySelector('img[src]'))imgCount++;if(links(it).length>0)linkCount++;}
const count=items.length;const avgText=totalText/count;
if(avgText<12)continue;
const tf=Math.min(Math.max(avgText/50,0.5),4);
const score=count*tf*(1+(extCount/count)*2+(hCount/count)+(imgCount/count)*0.5+(linkCount/count)*0.3);
if(score>bestScore){bestScore=score;best=c;bestKey=s;}
}
}
__JSONLD_FALLBACK__
if(!best){
if(IS_GITHUB&&isRepoPage()){const r=extractRepo();if(Object.keys(r).length>1)return JSON.stringify({container:'github-repo',detail:true,count:1,items:[r]});}
const ld=extractJsonLd();
if(ld)return JSON.stringify(ld);
return JSON.stringify({error:'no repeated list found',items:[]});
}
// Field extraction: clean, typed, deduplicated. No 'text' field.
// P1 counters: redirect unwraps and paid placements are reported in the
// envelope instead of silently reshaping the payload. They are counted below
// over the items RETURNED (after dedupe and the limit), never over every
// child: "10 of 5 items are paid placements" contradicted the payload.
const unwrappedSet=new WeakSet();
const items=[...best.children].filter(k=>k.nodeType===1&&isShown(k)&&groupKey(best,k)===bestKey&&isItem(k)).map(item=>{
if(item.tagName==='SHREDDIT-AD-POST'||item.querySelector('shreddit-ad-post'))return {};
const fullText=vtxt(item);const o={};
// Title: heading → longest link text → first sentence.
const h=item.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]');
let title=h?vtxt(h):'';
const link=bestLink(item,title);
if(link){const lt=vtxt(link);if(lt&&(!title||(!h&&title.length<10)))title=lt.slice(0,200);}
if(!title){const sentence=fullText.split(/\.|!|\?/)[0];title=(sentence&&sentence.length>10?sentence:fullText).slice(0,200);}
// Numeric/unit-like fragments ("2 units", "3 beds") are not titles.
if(title&&/^\s*[\d.,]+\s*(units?|beds?|baths?|ba|bd|mi|miles?|sq\.?\s?ft|sqft|acres?)?\s*$/i.test(title)){const lt2=fullText.replace(/\s+/g,' ').trim();if(lt2.length>title.length+6)title=lt2.slice(0,200);}
// A price glued into the title (listing-card text) is a field, not a name.
if(title){const pi=title.search(PRICE);if(pi>=10)title=title.slice(0,pi).trim();}
if(title)o.title=title.slice(0,200);
if(link){const cu=canonicalURL(link.href);if(cu!==link.href)unwrappedSet.add(o);o.url=cu;if(wrappedURL(cu))o.url_wrapped=true;}
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
const parts=[];let scanned=0;
const readDesc=n=>{if(++scanned>20000)return;if(n.nodeType===3){parts.push(n.textContent);return;}if(n.nodeType!==1)return;if(n.matches('a,h1,h2,h3,h4,h5,h6,script,style,noscript,svg,[hidden]'))return;const cs=n.ownerDocument.defaultView.getComputedStyle(n);if(cs.display==='none'||cs.visibility==='hidden'||cs.opacity==='0')return;for(const ch of n.childNodes)readDesc(ch);if(n.shadowRoot)for(const ch of n.shadowRoot.childNodes)readDesc(ch);};
readDesc(item);
const description=parts.join(' ').replace(/\s+/g,' ').trim();
if(description&&description.length>15){const dn=norm(description),tn=norm(title||'');if(dn&&!tn.includes(dn)&&!dn.includes(tn))o.description=description.slice(0,300);}
// Site-specific fields.
if(IS_REDDIT){const sp=item.tagName==='SHREDDIT-POST'?item:item.querySelector('shreddit-post');const sc0=item.tagName==='SHREDDIT-COMMENT'?item:item.querySelector('shreddit-comment');if(sp){const sps=sp.getAttribute('score');if(sps)o.fuzzed_score=parseInt(sps,10);const spc=sp.getAttribute('comment-count');if(spc)o.comments=parseInt(spc,10);const spa=sp.getAttribute('author');if(spa)o.author='u/'+spa;const spsub=sp.getAttribute('subreddit-prefixed-name');if(spsub)o.subreddit=spsub;const spt=sp.getAttribute('post-title');if(spt)o.title=spt.slice(0,200);const spl=sp.getAttribute('permalink');if(spl)o.url='https://www.reddit.com'+(spl.charAt(0)==='/'?spl:'/'+spl);const spts=sp.getAttribute('created-timestamp');if(spts)o.date=spts.slice(0,16).replace('T',' ');}else if(sc0){const sca=sc0.getAttribute('author');if(sca)o.author=/^\[?(deleted|removed)\]?$/i.test(sca)?'[deleted]':'u/'+sca;const scs=sc0.getAttribute('score');if(scs!==null&&scs!==''&&!isNaN(parseInt(scs,10)))o.fuzzed_score=parseInt(scs,10);const scd=sc0.getAttribute('depth');if(scd!==null&&scd!==''&&!isNaN(parseInt(scd,10)))o.depth=parseInt(scd,10);const scb=sc0.querySelector('[slot="comment"]');const sct=scb?vtxt(scb).slice(0,400):'';if(sct)o.text=sct;}else if(item.classList&&item.classList.contains('thing')){const g2=n=>item.getAttribute(n)||'';const gs2=g2('data-score');if(gs2)o.score=parseInt(gs2,10);const gc2=g2('data-comments-count');if(gc2)o.comments=parseInt(gc2,10);const ga2=g2('data-author');if(ga2)o.author='u/'+ga2;const gsb2=g2('data-subreddit');if(gsb2)o.subreddit='r/'+gsb2;const gp2=g2('data-permalink');if(gp2)o.url='https://www.reddit.com'+gp2;const gtl=item.querySelector('a.title');if(gtl)o.title=vtxt(gtl).slice(0,200);}else{const rsc=rdScore(item);if(rsc!==null)o.fuzzed_score=rsc;const rcm=rdComments(item);if(rcm!==null)o.comments=rcm;const rau=rdAuthor(item);if(rau)o.author=rau;const rsu=rdSub(item);if(rsu)o.subreddit=rsu;const rcl=item.querySelector('a[href*="/comments/"]');if(rcl){o.url=rcl.href;const rclt=vtxt(rcl);if(rclt&&rclt.length>5&&rclt.length<300)o.title=rclt.slice(0,200);}}}
else if(IS_GITHUB){const st=ghStars(item);if(st!==null)o.stars=st;const fk=ghForks(item);if(fk!==null)o.forks=fk;const sd=ghStarsToday(item);if(sd!==null)o.stars_today=sd;const lb=ghLabels(item);if(lb.length>0)o.labels=lb;const nm=ghNumber(item);if(nm!==null)o.number=nm;const gs=ghStatus(item);if(gs)o.status=gs;}
else{const rt=rating(item);if(rt!==null)o.rating=rt;const rv=reviews(item);if(rv!==null)o.reviews=rv;const av=avail(item);if(av)o.availability=av;const op=origPrice(item);if(op)o.original_price=op;if(isSponsored(item))o.sponsored=true;}
return o;
}).filter(o=>Object.keys(o).length>0);
// Dedupe, THEN apply the limit (deduping only the first `limit` rows came
// back short of the cap while more items remained): the same item is common
// on listing pages (variant cards, an ad resurfacing an organic result). Key
// by canonical URL, else by normalized title; keyless items stay (they have
// nothing to collide with).
const uniq=[];const seen=new Set();let dupes=0,keyless=0;
for(const it of items){const key=it.url?it.url.split('#')[0]:(it.title?'t:'+norm(it.title):'i:'+(keyless++));if(key&&seen.has(key)){dupes++;continue;}if(uniq.length>=__LIMIT__)break;if(key)seen.add(key);uniq.push(it);}
let sponsored=0,unwrapped=0,unresolved=0;
for(const it of uniq){if(it.sponsored)sponsored++;if(unwrappedSet.has(it))unwrapped++;if(it.url_wrapped)unresolved++;}
const lowConf=uniq.length>0&&uniq.every(o=>!o.url)&&(uniq.reduce((a,o)=>a+(o.title||'').length,0)/uniq.length)<25;
const out={container:best.tagName.toLowerCase(),count:uniq.length,items:uniq};
if(sponsored>0)out.sponsored=sponsored;
if(dupes>0)out.duplicates_removed=dupes;
if(unwrapped>0)out.redirects_unwrapped=unwrapped;if(unresolved>0){out.redirects_unresolved=unresolved;out.note=unresolved+' unresolved redirect URL(s); destination is opaque - follow the link and read its final URL';}
const claim=pageClaim();
if(claim){out.page_total=claim.total;if(claim.total>uniq.length){out.partial=true;if(claim.paged)out.paged=true;}}
if(lowConf){out.confidence='low';out.note='no clear list found — items may be page fragments; verify before relying on them';}
return JSON.stringify(out);
})()
