// X.com / Twitter adapter: GraphQL handoff + DOM card fast path for collect/fallback.
if (IS_X) {
  const X_NON_PROFILE = new Set([
    'home', 'explore', 'notifications', 'messages', 'settings', 'search',
    'compose', 'i', 'login', 'logout', 'signup', 'tos', 'privacy', 'intent',
    'hashtag', 'lists', 'bookmarks', 'communities', 'jobs', 'grok',
  ]);
  const X_PROFILE_TABS = new Set(['with_replies', 'media', 'likes', 'highlights', 'articles']);
  if (__POST_MARKER__) {
    const p = location.pathname.split('/').filter(Boolean);
    let xkind = 'other';
    if (p.indexOf('status') >= 0) xkind = 'status';
    else if (location.pathname.startsWith('/search')) xkind = 'search';
    else if (location.pathname === '/home') xkind = 'home';
    else if (p.length === 1 && !X_NON_PROFILE.has(p[0].toLowerCase())) xkind = 'profile';
    else if (p.length === 2 && !X_NON_PROFILE.has(p[0].toLowerCase()) && X_PROFILE_TABS.has(p[1].toLowerCase())) xkind = 'profile';
    return JSON.stringify({
      container: 'x-page',
      handoff: true,
      kind: xkind,
      url: location.href,
    });
  }
  // DOM fast path (used by `act collect` and when GraphQL handoff falls back).
  const articles = [...document.querySelectorAll('article[data-testid="tweet"]')];
  if (articles.length >= 1) {
    const seenIds = new Set();
    const items = [];
    for (const art of articles) {
      if (items.length >= __LIMIT__) break;
      const link = art.querySelector('a[href*="/status/"]');
      if (!link) continue;
      const m = (link.getAttribute('href') || '').match(/\/([^/]+)\/status\/(\d+)/);
      if (!m) continue;
      const id = m[2];
      if (seenIds.has(id)) continue;
      seenIds.add(id);
      const o = { id, url: 'https://x.com/' + m[1] + '/status/' + id };
      const un = vtxt(art.querySelector('[data-testid="User-Name"]'));
      const at = (un.match(/@([A-Za-z0-9_]+)/) || [])[1];
      o.author = at || m[1];
      const name = (un.split('\n')[0] || '').trim();
      if (name && name !== '@' + o.author) o.name = name;
      const tm = art.querySelector('time');
      if (tm && tm.getAttribute('datetime')) o.date = tm.getAttribute('datetime').slice(0, 16).replace('T', ' ') + ' UTC';
      const tw = art.querySelector('[data-testid="tweetText"]');
      const tx = tw ? vtxt(tw) : '';
      if (tx) o.text = tx.slice(0, 2000);
      const ariaNum = sel => {
        const el = art.querySelector(sel);
        const al = (el && el.getAttribute('aria-label')) || '';
        const nm = al.match(/(\d[\d.,]*\s*[KkMmBb]?)/);
        return nm ? parseCount(nm[1].replace(/\s+/g, '')) : null;
      };
      const rep = ariaNum('[data-testid="reply"]'); if (rep !== null) o.replies = rep;
      const rt = ariaNum('[data-testid="retweet"]'); if (rt !== null) o.reposts = rt;
      const lk = ariaNum('[data-testid="like"]'); if (lk !== null) o.likes = lk;
      const imgs = [...art.querySelectorAll('img[src*="pbs.twimg.com/media/"]')].map(i => i.src).filter(Boolean);
      if (imgs.length) o.media = imgs;
      items.push(o);
    }
    if (items.length > 0) {
      return JSON.stringify({ container: 'x-dom', count: items.length, items });
    }
  }
}
