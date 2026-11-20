// GitHub adapter: issues & PR lists, single issue/PR thread, releases, search, and repo root.
if (IS_GITHUB) {
  const ghParts = location.pathname.split('/').filter(Boolean);

  // 1. Single Issue or Pull Request detail page (/owner/repo/issues/N or /owner/repo/pull/N).
  if (ghParts.length >= 4 && (ghParts[2] === 'issues' || ghParts[2] === 'pull') && /^\d+$/.test(ghParts[3])) {
    const titleEl = document.querySelector('[data-testid="issue-title"], bdi.js-issue-title, h1 .js-issue-title');
    const title = titleEl ? vtxt(titleEl) : (document.querySelector('h1') ? vtxt(document.querySelector('h1')) : '');
    if (title) {
      const issue = {
        repo: ghParts[0] + '/' + ghParts[1],
        kind: ghParts[2] === 'pull' ? 'pr' : 'issue',
        number: parseInt(ghParts[3], 10),
        title: title.replace(/\s*#\d+\s*$/, '').trim().slice(0, 300),
        url: location.origin + '/' + ghParts.slice(0, 4).join('/'),
      };
      const stEl = document.querySelector('[data-testid="header-state"], .State, [title^="Status:"]');
      if (stEl && vtxt(stEl)) issue.state = vtxt(stEl).toLowerCase();
      const authorEl = document.querySelector('[data-testid="issue-body-header-author"], .gh-header-meta a.author, .timeline-comment a.author');
      if (authorEl && vtxt(authorEl)) issue.author = vtxt(authorEl);
      const timeEl = document.querySelector('[data-testid="issue-body"] relative-time, .gh-header-meta relative-time');
      if (timeEl) issue.date = timeEl.getAttribute('datetime') || vtxt(timeEl);
      const labels = [...document.querySelectorAll('[data-testid="issue-labels"] a, .js-issue-labels a, a[href*="/labels/"]')].map(a => vtxt(a)).filter(Boolean);
      if (labels.length) issue.labels = [...new Set(labels)].slice(0, 20);
      const bodyEl = document.querySelector('[data-testid="issue-body-viewer"] [data-testid="markdown-body"], [data-testid="issue-body"] .markdown-body, .js-comment-body .markdown-body, .comment-body.markdown-body');
      if (bodyEl && vtxt(bodyEl)) issue.body = vtxt(bodyEl).slice(0, 4000);

      // Comments (new React viewer + classic timeline-comment).
      const items = [];
      const seenBodies = new Set();
      if (bodyEl) seenBodies.add(bodyEl);
      const commentBoxes = [...document.querySelectorAll('[data-testid^="comment-viewer-outer-box-"], .js-timeline-item .timeline-comment')];
      for (const box of commentBoxes) {
        if (items.length >= __LIMIT__) break;
        const mb = box.querySelector('[data-testid="markdown-body"], .markdown-body');
        if (!mb || seenBodies.has(mb)) continue;
        seenBodies.add(mb);
        const text = vtxt(mb);
        if (!text) continue;
        const c = {};
        const au = box.querySelector('[data-testid="comment-subject-author"], [data-testid="avatar-link"], a.author');
        if (au && vtxt(au)) c.author = vtxt(au);
        const dt = box.querySelector('relative-time');
        if (dt) c.date = dt.getAttribute('datetime') || vtxt(dt);
        c.text = text.slice(0, 2000);
        items.push(c);
      }
      const hasMore = !!document.querySelector('[data-testid^="issue-timeline-load-more"], button.ajax-pagination-btn');
      const out = {
        container: 'github-issue',
        detail: true,
        issue,
        count: items.length,
        complete: !hasMore && commentBoxes.length <= items.length + 1,
        items,
      };
      if (!out.complete) {
        out.partial = true;
        out.note = hasMore
          ? 'older timeline comments are collapsed behind "Load more"'
          : 'comment cap reached (raise limit for more)';
      }
      return JSON.stringify(out);
    }
  }

  // 2. Issues & Pull Requests lists (React Issues, React PRs, and classic .js-issue-row).
  const tl = [
    ...document.querySelectorAll(
      'a[data-testid="issue-pr-title-link"], a[data-testid="issue-listitem-title-link"], a[data-testid="listitem-title-link"]'
    ),
  ];
  const classicRows = tl.length === 0 ? [...document.querySelectorAll('.js-issue-row a.js-navigation-open[id^="issue_"]')] : [];
  const listLinks = tl.length > 0 ? tl : classicRows;
  if (listLinks.length >= 1 && (ghParts[2] === 'issues' || ghParts[2] === 'pulls' || listLinks.length >= 3)) {
    const items = listLinks.slice(0, __LIMIT__).map(a => {
      const o = {};
      const t = vtxt(a);
      if (t) o.title = t.slice(0, 200);
      const h = a.getAttribute('href') || '';
      if (h) o.url = absURL(h);
      const m = h.match(/\/(?:issues|pull)\/(\d+)/);
      if (m) o.number = parseInt(m[1], 10);
      const row = a.closest('li, .js-issue-row, [role="listitem"]') || a.closest('div');
      if (row) {
        const lb = [...row.querySelectorAll('a[href*="label%3A"], a[href*="/labels/"], .IssueLabel')]
          .map(e => (e.innerText || '').split('\n')[0].trim())
          .filter(n => n && n.length > 1);
        if (lb.length) o.labels = [...new Set(lb)];
        const us = row.querySelector('a[data-hovercard-type="user"], a[data-testid="created-by-author"]');
        const ut = us ? vtxt(us) : '';
        if (ut) o.author = ut;
        const dt = row.querySelector('relative-time');
        if (dt) o.date = dt.getAttribute('datetime') || vtxt(dt);
        if (row.querySelector('svg[class*="octicon-issue-open"]')) o.status = 'open';
        else if (row.querySelector('svg[class*="octicon-issue-closed"], svg[class*="octicon-skip"]')) o.status = 'closed';
        else if (row.querySelector('svg[class*="octicon-git-merge"]')) o.status = 'merged';
        else if (row.querySelector('svg[class*="octicon-git-pull-request-draft"]')) o.status = 'draft';
        else if (row.querySelector('svg[class*="octicon-git-pull-request"]')) o.status = 'open';
      }
      return o;
    }).filter(o => o.title || o.url);
    if (items.length > 0) {
      return JSON.stringify({
        container: ghParts[2] === 'pulls' ? 'github-pulls' : 'github-issues',
        count: items.length,
        items,
      });
    }
  }

  // 3. GitHub Releases list (/owner/repo/releases).
  if (ghParts.length >= 3 && ghParts[2] === 'releases' && ghParts.length === 3) {
    const tagLinks = [...document.querySelectorAll('a[href*="/releases/tag/"]')].filter(a => {
      const t = vtxt(a);
      return t && !/^read more$/i.test(t) && !a.closest('nav');
    });
    const seenTags = new Set();
    const items = [];
    for (const a of tagLinks) {
      if (items.length >= __LIMIT__) break;
      const href = absURL(a.getAttribute('href') || '');
      if (!href || seenTags.has(href)) continue;
      const card = a.closest('section, div[data-test-selector="release-card"], .Box') || a.closest('div');
      if (!card) continue;
      seenTags.add(href);
      const o = { title: vtxt(a).slice(0, 200), url: href };
      const tm = href.match(/\/releases\/tag\/([^/?#]+)/);
      if (tm) o.tag = decodeURIComponent(tm[1]);
      const dt = card.querySelector('relative-time');
      if (dt) o.date = dt.getAttribute('datetime') || vtxt(dt);
      if (card.querySelector('.Label--success') || /\bLatest\b/.test(vtxt(card).slice(0, 120))) o.latest = true;
      const body = card.querySelector('.markdown-body');
      if (body && vtxt(body)) o.notes = vtxt(body).slice(0, 500);
      items.push(o);
    }
    if (items.length > 0) {
      return JSON.stringify({ container: 'github-releases', count: items.length, items });
    }
  }

  // 4. GitHub Search results (/search?q=...).
  if (ghParts[0] === 'search') {
    const list = document.querySelector('[data-testid="results-list"]');
    if (list) {
      const cards = [...list.children].filter(el => el.nodeType === 1 && isShown(el));
      const items = cards.slice(0, __LIMIT__).map(card => {
        const o = {};
        const link = card.querySelector('h3 a[href], a[data-testid="search-result-link"], a[href^="/"]');
        if (link) {
          const t = vtxt(link);
          if (t) o.title = t.slice(0, 200);
          const u = absURL(link.getAttribute('href') || '');
          if (u) o.url = u;
        }
        const descEl = card.querySelector('span.search-match, p');
        if (descEl && vtxt(descEl) && vtxt(descEl) !== o.title) o.description = vtxt(descEl).slice(0, 300);
        const st = ghStars(card); if (st !== null) o.stars = st;
        const topics = [...card.querySelectorAll('a[href*="/topics/"]')].map(a => vtxt(a)).filter(Boolean);
        if (topics.length) o.topics = topics.slice(0, 8);
        const full = vtxt(card);
        const upd = full.match(/Updated\s+([^·\n]+)/i);
        if (upd) o.updated = upd[1].trim();
        return o;
      }).filter(o => o.title && o.url);
      if (items.length > 0) {
        const q = new URLSearchParams(location.search).get('q') || '';
        const out = { container: 'github-search', count: items.length, items };
        if (q) out.query = q;
        return JSON.stringify(out);
      }
    }
  }

  // 5. Repo root: the repo itself is the payload.
  if (isRepoPage() && ghParts.length === 2) {
    const rr = extractRepo();
    if (Object.keys(rr).length > 1) {
      return JSON.stringify({ container: 'github-repo', detail: true, count: 1, items: [rr] });
    }
  }
}
