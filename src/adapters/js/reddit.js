// Reddit adapter: post/search Rust handoffs + shreddit & old.reddit DOM fast paths.
if (IS_REDDIT) {
  const isCp = location.pathname.indexOf('/comments/') >= 0;
  // Post pages: hand off to the Rust comment sweep (src/reddit.rs).
  if (__POST_MARKER__ && isCp) {
    const rbase = (location.pathname.match(/^(.*?\/comments\/[a-z0-9]+)/i) || [])[1];
    if (rbase) {
      let rsort = new URLSearchParams(location.search).get('sort') || 'confidence';
      const rsel = document.querySelector('[aria-selected=true],[aria-checked=true]');
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
        permalink: rbase,
        sort: rsort,
      });
    }
  }
  // Search pages: hand off to the Rust listing sweep (src/reddit/search.rs).
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
  const posts = [...document.querySelectorAll('shreddit-post')];
  if (isCp && cmts.length >= 1) {
    const items = cmts.slice(0, __LIMIT__).map(sc => {
      const o = {};
      const a = n => sc.getAttribute(n) || '';
      const au = a('author'); if (au) o.author = 'u/' + au;
      const s = a('score'); if (s !== '' && !isNaN(parseInt(s, 10))) o.fuzzed_score = parseInt(s, 10);
      const d = a('depth'); if (d !== '' && !isNaN(parseInt(d, 10))) o.depth = parseInt(d, 10);
      const pl = a('permalink'); if (pl) o.url = 'https://www.reddit.com' + (pl.charAt(0) === '/' ? pl : '/' + pl);
      const cr = a('created'); if (cr) o.date = cr.slice(0, 16).replace('T', ' ');
      const bd = sc.querySelector('[slot="comment"]');
      const bt = bd ? vtxt(bd) : '';
      if (bt) o.text = bt.slice(0, 500);
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
  if (!isCp && posts.length >= 1) {
    const items = posts.slice(0, __LIMIT__).map(sp => {
      const o = {};
      const a = n => sp.getAttribute(n) || '';
      const pt = a('post-title'); if (pt) o.title = pt.slice(0, 200);
      const pl = a('permalink'); if (pl) o.url = 'https://www.reddit.com' + (pl.charAt(0) === '/' ? pl : '/' + pl);
      const s = a('score'); if (s !== '' && !isNaN(parseInt(s, 10))) o.fuzzed_score = parseInt(s, 10);
      const cc = a('comment-count'); if (cc !== '' && !isNaN(parseInt(cc, 10))) o.comments = parseInt(cc, 10);
      const au = a('author'); if (au) o.author = 'u/' + au;
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
        if (pl) o.url = 'https://www.reddit.com' + (pl.charAt(0) === '/' ? pl : '/' + pl);
        const sc = a('data-score'); if (sc !== '' && !isNaN(parseInt(sc, 10))) o.score = parseInt(sc, 10);
        const cc = a('data-comments-count'); if (cc !== '' && !isNaN(parseInt(cc, 10))) o.comments = parseInt(cc, 10);
        const au = a('data-author'); if (au) o.author = 'u/' + au;
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
