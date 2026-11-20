(async () => {
  const cap = __CAP__;
  const items = new Map();
  function txt(e) { return e ? (e.innerText || e.textContent || '').trim() : ''; }
  function aria(a, sel) { const e = a.querySelector(sel); return (e && e.getAttribute('aria-label')) || ''; }
  function collect() {
    for (const a of document.querySelectorAll('article[data-testid="tweet"]')) {
      const link = a.querySelector('a[href*="/status/"]');
      if (!link) continue;
      const m = (link.getAttribute('href') || '').match(/\/([^/]+)\/status\/(\d+)/);
      if (!m) continue;
      const id = m[2];
      if (items.has(id)) continue;
      const un = txt(a.querySelector('[data-testid="User-Name"]'));
      const at = (un.match(/@[A-Za-z0-9_]+/) || [''])[0];
      const t = a.querySelector('time');
      const media = [...a.querySelectorAll('img[src*="pbs.twimg.com/media/"]')]
        .map(img => img.src)
        .filter(Boolean);
      items.set(id, {
        id: id,
        url: 'https://x.com/' + m[1] + '/status/' + id,
        author: at.replace('@', '') || m[1],
        name: (un.split('\n')[0] || ''),
        date: t ? (t.getAttribute('datetime') || '') : '',
        text: txt(a.querySelector('[data-testid="tweetText"]')),
        replies: aria(a, '[data-testid="reply"]'),
        reposts: aria(a, '[data-testid="retweet"]'),
        likes: aria(a, '[data-testid="like"]'),
        media: media,
      });
    }
  }
  collect();
  let idle = 0, steps = 0;
  const maxSteps = 40;
  while (items.size < cap && idle < 6 && steps < maxSteps) {
    const before = items.size;
    window.scrollBy(0, Math.round(window.innerHeight * 0.9));
    await new Promise(r => setTimeout(r, 350));
    collect();
    steps++;
    if (items.size === before) idle++; else idle = 0;
  }
  return JSON.stringify({
    count: items.size,
    exhausted: idle >= 6,
    hit_step_cap: steps >= maxSteps && idle < 6 && items.size < cap,
    items: [...items.values()].slice(0, cap),
  });
})()
