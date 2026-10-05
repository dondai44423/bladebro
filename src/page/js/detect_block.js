(()=>{const d=document;if(!d)return null;const t=(d.title||'').toLowerCase();const h=(location.hostname||'').toLowerCase();const raw=(()=>{if(!d.body)return'';if(d.querySelectorAll('*').length<=1500)return d.body.innerText||d.body.textContent||'';let s='';const w=n=>{if(s.length>1600)return;for(const c of n.childNodes){if(s.length>1600)return;if(c.nodeType===3)s+=c.textContent;else if(c.nodeType===1){const tg=c.tagName;if(tg==='SCRIPT'||tg==='STYLE'||tg==='NOSCRIPT'||tg==='TEMPLATE')continue;if(c.shadowRoot)w(c.shadowRoot);w(c);}}};w(d.body);return s;})();const body=raw.toLowerCase();const bl=body.length;const q=s=>!!d.querySelector(s);
// Cloudflare interstitial: the title is the strongest signal. A bare
// turnstile/challenge-platform SCRIPT is NOT — sites embed Turnstile
// widgets in ordinary forms. Only call it a block when the title matches
// or the classic challenge form is present or (widget present AND the
// page is nearly empty — a real interstitial).
if(t.includes('just a moment'))return 'cloudflare';
if(q('#challenge-form'))return 'cloudflare';
if(q('cf-turnstile')||q('script[src*=challenge-platform]')){if(bl<800)return 'cloudflare';return null;}
if(q('iframe[src*=captcha-delivery]')&&bl<1200)return 'datadome';
if(bl<1200&&body.includes('datadome')&&q('iframe'))return 'datadome';
if((q('#px-captcha')||q('script[src*=px-captcha]'))&&bl<1500)return 'perimeterx';
// Reddit JS challenge — a tiny hidden auto-submitting form (a real browser
// solves it in ~1s; solution = the token doubled). Not a wall: it clears
// itself, so nav waits it out instead of reporting a block.
if(bl<2000&&(q('input[name=js_challenge]')||q('input[name=jsc_token]')))return 'js-challenge';
// Reddit network-security wall — small page, no title: "You've been
// blocked by network security" / classic "whoa there, pardner!". Soft and
// transient (its 403 carries retry-after: 0); a reload clears it.
if(bl<1500&&(body.includes('blocked by network security')||body.includes('whoa there')))return 'reddit';
// Reddit's one-time humanity check: a reCAPTCHA v2 checkbox on a small
// reddit page ("Prove your humanity"). One humanized click passes it
// (verified live); the solve grants `loid` and the wall does not return
// for that profile. Host-gated so other sites' recaptchas stay untouched.
if(bl<1500&&h.indexOf('reddit.com')>=0&&(q('.g-recaptcha')||q('iframe[src*=recaptcha]'))&&(t.includes('humanity')||body.includes('prove your humanity')))return 'reddit-humanity';
// reCAPTCHA wall: needs BOTH the widget AND the "prove you're human"
// phrasing on a SMALL page (a contact page with a recaptcha + an FAQ
// mentioning robots is a normal page).
if((q('.g-recaptcha')||q('iframe[src*=recaptcha]'))&&(t.includes('prove')||t.includes('humanity')||t.includes('robot')||body.includes('prove your humanity')||body.includes('are you human'))&&bl<1500)return 'recaptcha';
if(bl<1000&&body.includes('access denied')&&(body.includes('reference')||body.includes('akamai')))return 'akamai';
// Rate limit: API docs routinely contain the phrase "rate limit" in long
// bodies. A real 429 wall is tiny.
if(bl<800&&(body.includes('too many requests')||body.includes('rate limit')))return 'rate-limit';
return null;})()