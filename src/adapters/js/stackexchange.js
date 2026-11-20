// Stack Overflow & Stack Exchange Network adapter: question+answers detail AND question listings.
const IS_SE =
  hostMatches('stackoverflow.com') ||
  hostMatches('stackexchange.com') ||
  hostMatches('serverfault.com') ||
  hostMatches('superuser.com') ||
  hostMatches('askubuntu.com') ||
  hostMatches('mathoverflow.net');

if (IS_SE) {
  const qEl = document.querySelector('#question');
  if (qEl && /^\/questions\/\d+/.test(location.pathname)) {
    const titleEl =
      document.querySelector('#question-header h1 a, #question-header h1, h1[itemprop="name"]') ||
      [...document.querySelectorAll('h1')].find(h => isShown(h) && vtxt(h).length > 3);
    const title = titleEl ? vtxt(titleEl) : document.title.replace(/\s*-\s*[^-]+$/, '').trim();
    const question = {
      title: title.slice(0, 300),
      url: location.href.split('#')[0],
    };
    const qScore = qEl.getAttribute('data-score') || vtxt(qEl.querySelector('.js-vote-count, [itemprop="upvoteCount"]'));
    if (qScore && /^-?\d+$/.test(qScore.trim())) question.score = parseInt(qScore.trim(), 10);
    const tags = [...qEl.querySelectorAll('.post-tag, .js-post-tag-list-item a')]
      .map(a => vtxt(a))
      .filter(Boolean);
    if (tags.length) question.tags = [...new Set(tags)];
    const qBody = qEl.querySelector('.js-post-body, [itemprop="text"]');
    if (qBody && vtxt(qBody)) question.body = vtxt(qBody).slice(0, 4000);
    const qAuthor = [...qEl.querySelectorAll('.post-signature .user-details a')].pop();
    if (qAuthor && vtxt(qAuthor)) question.author = vtxt(qAuthor);
    const asked = document.querySelector('time[itemprop="dateCreated"]');
    if (asked) question.asked = asked.getAttribute('datetime') || vtxt(asked);

    const ansEls = [...document.querySelectorAll('#answers .answer, #answers .js-answer')];
    const items = ansEls.slice(0, __LIMIT__).map(a => {
      const o = {};
      const aid = a.getAttribute('data-answerid') || '';
      if (aid) {
        o.id = aid;
        o.url = location.origin + '/a/' + aid;
      }
      const sc = a.getAttribute('data-score') || vtxt(a.querySelector('.js-vote-count, [itemprop="upvoteCount"]'));
      if (sc && /^-?\d+$/.test(sc.trim())) o.score = parseInt(sc.trim(), 10);
      if (a.classList.contains('accepted-answer') || a.getAttribute('itemprop') === 'acceptedAnswer') {
        o.accepted = true;
      }
      const au = [...a.querySelectorAll('.post-signature .user-details a')].pop();
      if (au && vtxt(au)) o.author = vtxt(au);
      const dt = a.querySelector('.post-signature .relativetime');
      if (dt) o.date = dt.getAttribute('title') || vtxt(dt);
      const body = a.querySelector('.js-post-body, [itemprop="text"]');
      if (body && vtxt(body)) o.text = vtxt(body).slice(0, 3000);
      return o;
    }).filter(o => o.text);

    const hasNextPage = !!document.querySelector('#answers .s-pagination--item[rel="next"]');
    const out = {
      container: 'stackexchange-question',
      detail: true,
      question,
      count: items.length,
      complete: !hasNextPage && ansEls.length <= items.length,
      items,
    };
    if (!out.complete) {
      out.partial = true;
      out.note = hasNextPage
        ? 'more answers exist on subsequent pages'
        : 'answer cap reached (raise limit for more)';
    }
    return JSON.stringify(out);
  }

  // Question listings (/questions, /questions/tagged/..., /search).
  const summaries = [...document.querySelectorAll('.s-post-summary')].filter(isShown);
  if (summaries.length >= 1) {
    const items = summaries.slice(0, __LIMIT__).map(el => {
      const o = {};
      const link = el.querySelector('.s-post-summary--content-title a[href]');
      if (link) {
        o.title = vtxt(link).slice(0, 200);
        o.url = absURL(link.getAttribute('href') || '');
      }
      for (const st of el.querySelectorAll('.s-post-summary--stats-item')) {
        const t = vtxt(st);
        const m = t.match(/(-?\d[\d.,]*[KkMm]?)\s*(votes?|answers?|views?)/i);
        if (!m) continue;
        const n = parseCount(m[1]);
        if (n === null) continue;
        const k = m[2].toLowerCase();
        if (k.startsWith('vote')) o.votes = n;
        else if (k.startsWith('answer')) {
          o.answers = n;
          if (st.classList.contains('has-accepted-answer')) o.accepted = true;
        } else if (k.startsWith('view')) o.views = n;
      }
      const tags = [...el.querySelectorAll('.s-post-summary--meta-tags a')].map(a => vtxt(a)).filter(Boolean);
      if (tags.length) o.tags = [...new Set(tags)];
      const exc = el.querySelector('.s-post-summary--content-excerpt');
      if (exc && vtxt(exc)) o.excerpt = vtxt(exc).slice(0, 280);
      const au = el.querySelector('.s-user-card--link a');
      if (au && vtxt(au)) o.author = vtxt(au);
      const dt = el.querySelector('.relativetime');
      if (dt) o.date = dt.getAttribute('title') || vtxt(dt);
      return o;
    }).filter(o => o.title && o.url);
    if (items.length > 0) {
      return JSON.stringify({ container: 'stackexchange-questions', count: items.length, items });
    }
  }
}
