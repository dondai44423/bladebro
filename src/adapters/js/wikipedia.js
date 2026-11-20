// Wikipedia adapter: article pages (summary + infobox + sections) and search results.
if (hostMatches('wikipedia.org')) {
  const searchRows = [...document.querySelectorAll('.mw-search-result')].filter(isShown);
  if (searchRows.length >= 1) {
    const items = searchRows.slice(0, __LIMIT__).map(r => {
      const o = {};
      const a = r.querySelector('.mw-search-result-heading a[href]');
      if (a) {
        o.title = vtxt(a).slice(0, 200);
        o.url = absURL(a.getAttribute('href') || '');
      }
      const sn = r.querySelector('.searchresult');
      if (sn && vtxt(sn)) o.snippet = vtxt(sn).slice(0, 320);
      const dt = r.querySelector('.mw-search-result-data');
      if (dt && vtxt(dt)) o.meta = vtxt(dt).slice(0, 120);
      return o;
    }).filter(o => o.title && o.url);
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('search') || '';
      const out = { container: 'wikipedia-search', count: items.length, items };
      if (q) out.query = q;
      return JSON.stringify(out);
    }
  }

  if (location.pathname.startsWith('/wiki/') && !/^\/wiki\/Special:/i.test(location.pathname)) {
    const h1 = document.querySelector('#firstHeading, h1');
    const title = h1 ? vtxt(h1) : '';
    const content = document.querySelector('#mw-content-text .mw-parser-output');
    if (title && content) {
      const article = {
        title: title.slice(0, 200),
        url: location.href.split('#')[0],
      };
      // Lead paragraphs (skip infobox/table/hatnote/empty paragraphs).
      const paras = [...content.querySelectorAll('p')]
        .filter(p => !p.closest('table, .infobox, .hatnote, .mbox-text, .navbox, .toc') && vtxt(p).length > 40)
        .slice(0, 3)
        .map(p => vtxt(p).replace(/\[\d+\]/g, '').trim());
      if (paras.length) article.summary = paras.join('\n\n').slice(0, 2000);

      // Infobox key-value pairs.
      const ib = content.querySelector('table.infobox');
      if (ib) {
        const info = {};
        let rows = 0;
        for (const tr of ib.querySelectorAll('tr')) {
          if (rows >= 20) break;
          const th = tr.querySelector('th');
          const td = tr.querySelector('td');
          if (!th || !td) continue;
          const k = vtxt(th).replace(/\u00a0/g, ' ').trim();
          const v = vtxt(td).replace(/\u00a0/g, ' ').replace(/\[\d+\]/g, '').trim();
          if (k && v && k.length <= 60) {
            info[k] = v.slice(0, 200);
            rows++;
          }
        }
        if (rows > 0) article.infobox = info;
      }

      // Top-level sections.
      const secs = [...content.querySelectorAll('.mw-heading h2, h2 .mw-headline, h2')]
        .map(h => vtxt(h).replace(/\[edit\]$/i, '').trim())
        .filter(s => s && s !== 'Contents');
      if (secs.length) article.sections = [...new Set(secs)].slice(0, 30);

      const cats = [...document.querySelectorAll('#mw-normal-catlinks ul li a')]
        .map(a => vtxt(a))
        .filter(Boolean);
      if (cats.length) article.categories = cats.slice(0, 15);

      return JSON.stringify({
        container: 'wikipedia-article',
        detail: true,
        count: 1,
        items: [article],
      });
    }
  }
}
