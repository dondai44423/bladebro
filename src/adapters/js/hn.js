// Hacker News adapter: story listings AND `/item?id=` discussion threads.
if (IS_HN) {
  const isItemPage = location.pathname === '/item' || !!document.querySelector('table.fatitem');
  if (isItemPage) {
    const fat = document.querySelector('table.fatitem');
    const subRow = fat ? fat.querySelector('tr.athing') : document.querySelector('tr.athing.submission');
    const post = {};
    if (subRow) {
      const sid = subRow.getAttribute('id') || new URLSearchParams(location.search).get('id') || '';
      if (sid) {
        post.id = sid;
        post.hn_url = location.origin + '/item?id=' + sid;
      }
      const ta = subRow.querySelector('span.titleline > a') || subRow.querySelector('a.titlelink');
      if (ta) {
        const t = vtxt(ta);
        if (t) post.title = t.slice(0, 300);
        const h = ta.getAttribute('href') || '';
        if (h) post.url = absURL(h);
      }
      const site = subRow.querySelector('span.sitestr');
      if (site && vtxt(site)) post.site = vtxt(site);
      const st = subRow.nextElementSibling ? subRow.nextElementSibling.querySelector('.subtext') : null;
      if (st) {
        const sc = st.querySelector('.score');
        if (sc) {
          const m = vtxt(sc).replace(/\u00a0/g, ' ').match(/(\d[\d,]*)\s*point/i);
          if (m) post.points = parseInt(m[1].replace(/,/g, ''), 10);
        }
        const au = st.querySelector('a.hnuser, a[href^="user?id="]');
        if (au && vtxt(au)) post.author = vtxt(au);
        const ag = st.querySelector('span.age a');
        if (ag && vtxt(ag)) post.age = vtxt(ag);
        const cl = [...st.querySelectorAll('a')].find(a => /comment|discuss/i.test(vtxt(a)));
        if (cl) {
          const m = vtxt(cl).replace(/\u00a0/g, ' ').match(/(\d[\d,]*)/);
          post.comments = m ? parseInt(m[1].replace(/,/g, ''), 10) : 0;
        }
      }
      const topText = fat ? fat.querySelector('.toptext') : null;
      if (topText && vtxt(topText)) post.text = vtxt(topText).slice(0, 4000);
    }
    const comRows = [...document.querySelectorAll('tr.athing.comtr')];
    const items = comRows.slice(0, __LIMIT__).map(tr => {
      const o = {};
      const cid = tr.getAttribute('id') || '';
      if (cid) {
        o.id = cid;
        o.url = location.origin + '/item?id=' + cid;
      }
      const ind = tr.querySelector('td.ind');
      if (ind) {
        const ia = ind.getAttribute('indent');
        if (ia !== null && ia !== '' && !isNaN(parseInt(ia, 10))) {
          o.depth = parseInt(ia, 10);
        } else {
          const img = ind.querySelector('img');
          if (img && img.width !== undefined) o.depth = Math.round((parseInt(img.width, 10) || 0) / 40);
        }
      }
      const au = tr.querySelector('a.hnuser, .comhead a[href^="user?id="], a[href^="user?id="]');
      if (au && vtxt(au)) o.author = vtxt(au);
      const ag = tr.querySelector('span.age a');
      if (ag && vtxt(ag)) o.age = vtxt(ag);
      const ct = tr.querySelector('.commtext');
      if (ct) {
        // Clone-free clean text: skip the trailing "reply" link if nested.
        const t = vtxt(ct);
        if (t) o.text = t.slice(0, 2000);
      }
      return o;
    }).filter(o => o.text || o.author);
    if (Object.keys(post).length > 0 || items.length > 0) {
      const out = {
        container: 'hn-comments',
        detail: true,
        post,
        count: items.length,
        total_in_dom: comRows.length,
        complete: comRows.length <= items.length && !document.querySelector('a.morelink'),
        items,
      };
      if (!out.complete) {
        out.partial = true;
        out.note = comRows.length > items.length
          ? 'capped at ' + items.length + ' of ' + comRows.length + ' comments in DOM (raise limit for more)'
          : 'more comments exist on the next page (follow More link)';
      }
      return JSON.stringify(out);
    }
  } else {
    const rows = [...document.querySelectorAll('tr.athing:not(.comtr)')];
    if (rows.length >= 1) {
      const items = rows.slice(0, __LIMIT__).map(tr => {
        const o = {};
        const id = tr.getAttribute('id') || '';
        const rk = tr.querySelector('span.rank');
        if (rk) {
          const rm = vtxt(rk).match(/(\d+)/);
          if (rm) o.rank = parseInt(rm[1], 10);
        }
        const ta = tr.querySelector('span.titleline > a') || tr.querySelector('a.titlelink');
        if (ta) {
          const t = vtxt(ta);
          if (t) o.title = t.slice(0, 200);
          const h = ta.getAttribute('href') || '';
          if (h) o.url = absURL(h);
        }
        const site = tr.querySelector('span.sitestr');
        if (site && vtxt(site)) o.site = vtxt(site);
        if (id) o.comments_url = location.origin + '/item?id=' + id;
        const st = tr.nextElementSibling ? tr.nextElementSibling.querySelector('.subtext') : null;
        if (st) {
          const sc = st.querySelector('.score');
          if (sc) {
            const m = vtxt(sc).replace(/\u00a0/g, ' ').match(/(\d[\d,]*)\s*point/i);
            if (m) o.points = parseInt(m[1].replace(/,/g, ''), 10);
          }
          const au = st.querySelector('a.hnuser, a[href^="user?id="]');
          if (au && vtxt(au)) o.author = vtxt(au);
          const ag = st.querySelector('span.age a');
          if (ag && vtxt(ag)) o.age = vtxt(ag);
          const cl = [...st.querySelectorAll('a')].find(a => /comment|discuss/i.test(vtxt(a)));
          if (cl) {
            const m = vtxt(cl).replace(/\u00a0/g, ' ').match(/(\d[\d,]*)/);
            o.comments = m ? parseInt(m[1].replace(/,/g, ''), 10) : 0;
          }
        }
        return o;
      }).filter(o => o.title);
      if (items.length > 0) {
        return JSON.stringify({ container: 'hn-items', count: items.length, items });
      }
    }
  }
}
