// Reddit adapter: post/search Rust handoffs + shreddit & old.reddit DOM fast paths.
if (IS_REDDIT) {
  const isCp = location.pathname.indexOf('/comments/') >= 0;
  const rdAuthorClean = raw => {
    const a = String(raw || '').trim().replace(/^\/?u\//i, '');
    if (!a || /^\[?(deleted|removed)\]?$/i.test(a)) return '[deleted]';
    return 'u/' + a;
  };
  const rdUrl = href => {
    if (!href) return '';
    const h = String(href).trim();
    if (/^https?:\/\//i.test(h)) return h;
    return 'https://www.reddit.com' + (h.charAt(0) === '/' ? h : '/' + h);
  };
  const rdOwned = (owner, sel, ownerSel) => {
    const list = owner.querySelectorAll(sel);
    for (let i = 0; i < list.length; i++) {
      if (typeof list[i].closest === 'function' && list[i].closest(ownerSel) === owner) return list[i];
    }
    return null;
  };
  // Post pages: hand off to the Rust HTML/DOM comment-tree sweep (src/reddit.rs).
  if (__POST_MARKER__ && isCp) {
    const rbase = (location.pathname.match(/^(.*?\/comments\/[a-z0-9]+)/i) || [])[1];
    if (rbase) {
      let rsort = new URLSearchParams(location.search).get('sort') || 'confidence';
      const rsel = document.querySelector('shreddit-sort-dropdown [aria-selected=true],shreddit-sort-dropdown [aria-checked=true],[aria-selected=true],[aria-checked=true]');
      if (rsel) {
        let rp = rsel;
        for (let ri = 0; ri < 6 && rp && rp !== document.body; ri++) {
          if (rp.tagName === 'DATA' && rp.getAttribute('value')) {
            rsort = rp.getAttribute('value').toLowerCase();
            break;
          }
          rp = rp.parentElement;
        }
      }
      return JSON.stringify({
        container: 'reddit-post-page',
        handoff: true,
        url: location.href,
        pathname: location.pathname,
        permalink: rbase,
        sort: rsort,
      });
    }
  }
  // Search pages: hand off to the Rust DOM/HTML search sweep (src/reddit/search.rs).
  if (__POST_MARKER__) {
    const spath = location.pathname;
    if (/\/search\/?$/.test(spath)) {
      const sqs = new URLSearchParams(location.search);
      const sty = sqs.get('type') || '';
      if ((sty === '' || sty === 'posts' || sty === 'links' || sty === 'all') && (sqs.get('q') || '').trim()) {
        return JSON.stringify({
          container: 'reddit-search-page',
          handoff: true,
          url: location.href,
          path: spath,
          params: location.search,
        });
      }
    }
  }
  const cmts = [...document.querySelectorAll('shreddit-comment')];
  const posts = [...document.querySelectorAll('shreddit-post')].filter(
    sp => !sp.hasAttribute('is-promoted') && (!sp.closest || !sp.closest('shreddit-ad-post'))
  );
  if (isCp && cmts.length >= 1) {
    const items = cmts.slice(0, __LIMIT__).map(sc => {
      const o = {};
      const a = n => sc.getAttribute(n) || '';
      const id = a('thingid').replace(/^t1_/i, ''); if (id) o.id = id;
      const au = sc.hasAttribute('is-author-deleted') ? '[deleted]' : a('author');
      if (au) o.author = rdAuthorClean(au);
      const s = a('score'); if (s !== '' && !isNaN(parseInt(s, 10))) o.fuzzed_score = parseInt(s, 10);
      const d = a('depth'); if (d !== '' && !isNaN(parseInt(d, 10))) o.depth = parseInt(d, 10);
      const pl = a('permalink'); if (pl) o.url = rdUrl(pl);
      let cr = a('created') || a('created-timestamp');
      if (!cr) {
        const ta = rdOwned(sc, 'faceplate-timeago[ts],time[datetime]', 'shreddit-comment');
        if (ta) cr = ta.getAttribute('ts') || ta.getAttribute('datetime') || '';
      }
      if (cr) o.date = cr.slice(0, 16).replace('T', ' ');
      const bd = rdOwned(sc, '[slot="comment"],[id$="-comment-rtjson-content"],[data-testid="comment"]', 'shreddit-comment');
      const bt = bd ? vtxt(bd) : (sc.hasAttribute('is-comment-removed') ? '[removed]' : (sc.hasAttribute('is-comment-deleted') ? '[deleted]' : ''));
      if (bt) o.text = bt.slice(0, 2000);
      return o;
    }).filter(o => Object.keys(o).length > 0);
    if (items.length > 0) {
      return JSON.stringify({
        container: 'reddit-comments',
        count: items.length,
        items,
        note: "fuzzed_score values are Reddit's displayed (fuzzed) scores",
      });
    }
  }
  // old.reddit.com comment page DOM fast path.
  if (isCp) {
    const oldCmts = [...document.querySelectorAll('.thing.comment')];
    if (oldCmts.length >= 1) {
      const items = oldCmts.slice(0, __LIMIT__).map(oc => {
        const o = {};
        const a = n => oc.getAttribute(n) || '';
        const id = a('data-fullname').replace(/^t1_/i, ''); if (id) o.id = id;
        const au = a('data-author'); if (au) o.author = rdAuthorClean(au);
        const scEl = rdOwned(oc, '.entry .score.unvoted', '.thing.comment');
        if (scEl) { const sv = parseInt((scEl.getAttribute('title') || vtxt(scEl)).replace(/,/g, ''), 10); if (!isNaN(sv)) o.score = sv; }
        const pl = a('data-permalink'); if (pl) o.url = rdUrl(pl);
        const tm = rdOwned(oc, '.entry time[datetime]', '.thing.comment');
        if (tm && tm.getAttribute('datetime')) o.date = tm.getAttribute('datetime').slice(0, 16).replace('T', ' ');
        const bd = rdOwned(oc, '.entry .usertext-body .md', '.thing.comment');
        const bt = bd ? vtxt(bd) : '';
        if (bt) o.text = bt.slice(0, 2000);
        return o;
      }).filter(o => o.text || o.author);
      if (items.length > 0) {
        return JSON.stringify({ container: 'reddit-comments', count: items.length, items });
      }
    }
  }
  // Shreddit search DOM fast path (e.g. inside act collect where handoffs are off).
  if (!isCp && /\/search\/?$/.test(location.pathname)) {
    const sunits = [...document.querySelectorAll('[data-testid="search-post-unit"],[data-testid="sdui-post-unit"]')].filter(
      u => !u.hasAttribute('is-promoted') && !u.querySelector('shreddit-ad-post,[data-testid*="promoted"]')
    );
    if (sunits.length >= 1) {
      const items = sunits.slice(0, __LIMIT__).map(u => {
        const o = {};
        const tl = u.querySelector('a[data-testid="post-title"],a[data-testid="post-title-text"],a[href*="/comments/"]');
        if (tl) {
          const t = vtxt(tl); if (t) o.title = t.slice(0, 200);
          const h = tl.getAttribute('href'); if (h) o.url = rdUrl(h);
        }
        const sl = u.querySelector('a[href^="/r/"]:not([href*="/comments/"])');
        if (sl) { const sm = (sl.getAttribute('href') || '').match(/\/r\/([A-Za-z0-9_+-]+)/); if (sm) o.subreddit = 'r/' + sm[1]; }
        const sc = rdScore(u); if (sc !== null) o.fuzzed_score = sc;
        const cc = rdComments(u); if (cc !== null) o.comments = cc;
        const tm = u.querySelector('faceplate-timeago[ts],time[datetime]');
        const cr = tm ? (tm.getAttribute('ts') || tm.getAttribute('datetime') || '') : '';
        if (cr) o.date = cr.slice(0, 16).replace('T', ' ');
        return o;
      }).filter(o => o.title && o.url);
      if (items.length > 0) {
        return JSON.stringify({ container: 'reddit-search', count: items.length, items });
      }
    }
  }
  if (!isCp && posts.length >= 1) {
    const items = posts.slice(0, __LIMIT__).map(sp => {
      const o = {};
      const a = n => sp.getAttribute(n) || '';
      const pt = a('post-title'); if (pt) o.title = pt.slice(0, 200);
      const pl = a('permalink'); if (pl) o.url = rdUrl(pl);
      const s = a('score'); if (s !== '' && !isNaN(parseInt(s, 10))) o.fuzzed_score = parseInt(s, 10);
      const cc = a('comment-count'); if (cc !== '' && !isNaN(parseInt(cc, 10))) o.comments = parseInt(cc, 10);
      const au = a('author'); if (au) o.author = rdAuthorClean(au);
      const sub = a('subreddit-prefixed-name') || a('subreddit-name');
      if (sub) o.subreddit = sub.indexOf('/') >= 0 ? sub : 'r/' + sub;
      const ts = a('created-timestamp'); if (ts) o.date = ts.slice(0, 16).replace('T', ' ');
      const dm = a('domain'); if (dm) o.domain = dm;
      const ty = a('post-type'); if (ty) o.type = ty;
      const ch = a('content-href'); if (ch) o.content_href = ch.slice(0, 300);
      return o;
    }).filter(o => Object.keys(o).length > 0);
    if (items.length > 0) {
      return JSON.stringify({
        container: 'reddit-feed',
        count: items.length,
        items,
        note: "fuzzed_score values are Reddit's displayed (fuzzed) scores",
      });
    }
  }
  // old.reddit.com feed fast path (.thing.link rows).
  if (!isCp) {
    const oldPosts = [...document.querySelectorAll('#siteTable .thing.link:not(.promoted)')];
    if (oldPosts.length >= 1) {
      const items = oldPosts.slice(0, __LIMIT__).map(el => {
        const o = {};
        const a = n => el.getAttribute(n) || '';
        const tl = el.querySelector('a.title');
        if (tl) o.title = vtxt(tl).slice(0, 200);
        const pl = a('data-permalink');
        if (pl) o.url = rdUrl(pl);
        const sc = a('data-score'); if (sc !== '' && !isNaN(parseInt(sc, 10))) o.score = parseInt(sc, 10);
        const cc = a('data-comments-count'); if (cc !== '' && !isNaN(parseInt(cc, 10))) o.comments = parseInt(cc, 10);
        const au = a('data-author'); if (au) o.author = rdAuthorClean(au);
        const sub = a('data-subreddit'); if (sub) o.subreddit = 'r/' + sub;
        const dm = a('data-domain'); if (dm) o.domain = dm;
        const tm = el.querySelector('time');
        if (tm && tm.getAttribute('datetime')) o.date = tm.getAttribute('datetime').slice(0, 16).replace('T', ' ');
        return o;
      }).filter(o => o.title);
      if (items.length > 0) {
        return JSON.stringify({ container: 'reddit-feed', count: items.length, items });
      }
    }
  }
}
