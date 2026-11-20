// YouTube adapter: search results, channel/home/playlist feeds, and watch pages.
if (hostMatches('youtube.com')) {
  const ytCleanUrl = href => {
    try {
      const u = new URL(href, location.origin);
      if (u.pathname === '/watch') {
        const v = u.searchParams.get('v');
        if (v) return 'https://www.youtube.com/watch?v=' + v;
      }
      if (u.pathname.startsWith('/shorts/')) {
        return 'https://www.youtube.com' + u.pathname;
      }
      u.searchParams.delete('pp');
      return u.href;
    } catch (e) {
      return absURL(href);
    }
  };

  const ytIsoDuration = iso => {
    const m = (iso || '').match(/^PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?$/i);
    if (!m) return null;
    const h = parseInt(m[1] || '0', 10);
    const min = parseInt(m[2] || '0', 10);
    const s = parseInt(m[3] || '0', 10);
    const pad = n => (n < 10 ? '0' + n : '' + n);
    return h > 0 ? (h + ':' + pad(min) + ':' + pad(s)) : (min + ':' + pad(s));
  };

  const ytReadCard = card => {
    if (card.closest('ytd-ad-slot-renderer, ytd-in-feed-ad-layout-renderer, ytd-promoted-video-renderer')) {
      return null;
    }
    const YT_CTA = /^(watch|play|watch now|play all|learn more|visit|open|view full playlist)$/i;
    let titleEl = card.querySelector('a#video-title, a#video-title-link, h3 a[href*="/watch"], h3 a[href*="/shorts/"]');
    if (!titleEl) {
      titleEl = [...card.querySelectorAll('a[href*="/watch"], a[href*="/shorts/"]')].find(a => {
        const t = (a.getAttribute('title') || vtxt(a)).trim();
        return t.length > 5 && !YT_CTA.test(t);
      });
    }
    if (!titleEl) return null;
    const title = (titleEl.getAttribute('title') || vtxt(titleEl)).trim();
    const rawHref = titleEl.getAttribute('href') || '';
    if (!title || YT_CTA.test(title) || !rawHref) return null;
    const o = { title: title.slice(0, 200), url: ytCleanUrl(rawHref) };
    const vm = o.url.match(/(?:[?&]v=|\/shorts\/)([A-Za-z0-9_-]{6,})/);
    if (vm) o.video_id = vm[1];
    if (o.url.includes('/shorts/')) o.shorts = true;
    const chEl = card.querySelector(
      '#channel-info ytd-channel-name a, ytd-channel-name a, #byline-container a, a[href^="/@"], a[href^="/channel/"], a[href^="/c/"]'
    );
    if (chEl && vtxt(chEl)) {
      o.channel = vtxt(chEl).slice(0, 120);
      const chHref = chEl.getAttribute('href') || '';
      if (chHref) o.channel_url = absURL(chHref);
    }
    const metas = [
      ...card.querySelectorAll(
        '#metadata-line span, .inline-metadata-item, yt-content-metadata-view-model span[role="text"]'
      ),
    ]
      .map(s => vtxt(s))
      .filter(s => s && s !== '•' && s !== o.channel);
    for (const mt of metas) {
      if (/view|watching/i.test(mt) || /^\d[\d.,]*[KkMmBb]$/.test(mt)) {
        if (!o.views) o.views = mt;
      } else if (/ago|streamed|premiered|live/i.test(mt)) {
        if (!o.published) o.published = mt;
      }
    }
    if (metas.length >= 2 && !o.views && !o.published) {
      o.views = metas[0];
      o.published = metas[1];
    }
    const durEl = card.querySelector(
      'ytd-thumbnail-overlay-time-status-renderer, badge-shape, span.ytd-thumbnail-overlay-time-status-renderer'
    );
    if (durEl) {
      const dt = vtxt(durEl);
      if (/^live$/i.test(dt)) o.live = true;
      else if (/^\d{1,2}:\d{2}(?::\d{2})?$/.test(dt)) o.duration = dt;
    }
    const snip = card.querySelector('.metadata-snippet-text, #description-text');
    if (snip && vtxt(snip)) o.snippet = vtxt(snip).slice(0, 300);
    return o;
  };

  // 1. Watch page (/watch?v=...).
  if (location.pathname === '/watch') {
    const vid = new URLSearchParams(location.search).get('v') || '';
    const titleEl = document.querySelector('ytd-watch-metadata h1, #title h1, h1.ytd-watch-metadata');
    const title =
      (titleEl && vtxt(titleEl)) ||
      (document.querySelector('meta[name="title"]')?.content || '').trim() ||
      (document.querySelector('meta[property="og:title"]')?.content || '').trim();
    if (title) {
      const video = {
        id: vid || undefined,
        title: title.slice(0, 300),
        url: vid ? ('https://www.youtube.com/watch?v=' + vid) : location.href,
      };
      const chEl = document.querySelector('ytd-watch-metadata ytd-channel-name a, #owner ytd-channel-name a');
      if (chEl && vtxt(chEl)) {
        video.channel = vtxt(chEl).slice(0, 120);
        const chHref = chEl.getAttribute('href') || '';
        if (chHref) video.channel_url = absURL(chHref);
      } else {
        const metaAuthor = document.querySelector('span[itemprop="author"] link[itemprop="name"]')?.getAttribute('content');
        if (metaAuthor) video.channel = metaAuthor.trim().slice(0, 120);
        const metaAuthorUrl = document.querySelector('span[itemprop="author"] link[itemprop="url"]')?.getAttribute('href');
        if (metaAuthorUrl) video.channel_url = metaAuthorUrl.replace(/^http:\/\//i, 'https://');
      }
      const subsEl = document.querySelector('#owner-sub-count');
      if (subsEl && vtxt(subsEl)) video.subscribers = vtxt(subsEl);
      // Avoid ytd-watch-info-text's rolling-digit spans; prefer itemprop meta or bold info tokens.
      const viewsMeta = document.querySelector('meta[itemprop="interactionCount"]')?.content;
      if (viewsMeta && /^\d+$/.test(viewsMeta)) {
        video.views = parseInt(viewsMeta, 10);
      } else {
        const infoSpans = [...document.querySelectorAll('#info-container span, ytd-watch-info-text span.bold, #description-inner span')]
          .map(s => vtxt(s))
          .filter(s => /\d[\d.,]*[KkMmBb]?\s+views?/i.test(s));
        if (infoSpans.length) video.views = infoSpans[0];
      }
      const dateMeta = document.querySelector('meta[itemprop="datePublished"]')?.content;
      if (dateMeta) video.published = dateMeta.slice(0, 10);
      const durMeta = document.querySelector('meta[itemprop="duration"]')?.content;
      const dur = ytIsoDuration(durMeta);
      if (dur) video.duration = dur;
      const likeBtn = document.querySelector('like-button-view-model button, #segmented-like-button button');
      if (likeBtn) {
        const al = likeBtn.getAttribute('aria-label') || '';
        const lm = al.match(/([\d,]+)\s+other/i) || al.match(/([\d.,]+[KkMmBb]?)/);
        if (lm) {
          const parsed = parseCount(lm[1]);
          if (parsed !== null) video.likes = parsed;
        }
      }
      const descEl = document.querySelector('#attributed-snippet-text, #description-inline-expander #expanded, #description-inline-expander');
      const descMeta = (document.querySelector('meta[name="description"]')?.content || '').trim();
      const descTxt = (descEl && vtxt(descEl)) || descMeta;
      if (descTxt) video.description = descTxt.replace(/\s*…\s*more$/i, '').slice(0, 2000);

      // Loaded comments (only what YouTube has rendered in the DOM).
      const cmtEls = [...document.querySelectorAll('ytd-comment-thread-renderer')];
      const items = cmtEls.slice(0, __LIMIT__).map(ct => {
        const o = {};
        const au = ct.querySelector('#author-text');
        if (au && vtxt(au)) o.author = vtxt(au);
        const dt = ct.querySelector('#published-time-text a, #published-time-text');
        if (dt && vtxt(dt)) o.date = vtxt(dt);
        const tx = ct.querySelector('#content-text');
        if (tx && vtxt(tx)) o.text = vtxt(tx).slice(0, 1500);
        const lk = ct.querySelector('#vote-count-middle');
        if (lk && vtxt(lk)) {
          const n = parseCount(vtxt(lk));
          if (n !== null) o.likes = n;
        }
        return o;
      }).filter(o => o.text);
      const out = {
        container: 'youtube-video',
        detail: true,
        video,
        count: items.length,
        items,
      };
      if (cmtEls.length === 0) {
        out.note = 'video metadata extracted; comments lazy-load on scroll';
      } else if (cmtEls.length > items.length) {
        out.partial = true;
      }
      return JSON.stringify(out);
    }
  }

  // 2. Search results (/results?search_query=...) — covers both ytd-video-renderer and yt-lockup-view-model.
  if (location.pathname === '/results') {
    const cards = [...document.querySelectorAll('ytd-video-renderer, yt-lockup-view-model')].filter(isShown);
    const seenUrls = new Set();
    const items = [];
    for (const c of cards) {
      if (items.length >= __LIMIT__) break;
      const item = ytReadCard(c);
      if (!item || seenUrls.has(item.url)) continue;
      seenUrls.add(item.url);
      items.push(item);
    }
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('search_query') || '';
      const out = { container: 'youtube-search', count: items.length, items };
      if (q) out.query = q;
      return JSON.stringify(out);
    }
  }

  // 3. Channel / Home / Playlist video grids & lists.
  const feedCards = [...document.querySelectorAll(
    'ytd-rich-item-renderer, ytd-grid-video-renderer, ytd-playlist-video-renderer, ytd-video-renderer, yt-lockup-view-model'
  )].filter(isShown);
  if (feedCards.length >= 1) {
    const seenUrls = new Set();
    const items = [];
    for (const c of feedCards) {
      if (items.length >= __LIMIT__) break;
      const item = ytReadCard(c);
      if (!item || seenUrls.has(item.url)) continue;
      seenUrls.add(item.url);
      items.push(item);
    }
    if (items.length > 0) {
      return JSON.stringify({ container: 'youtube-videos', count: items.length, items });
    }
  }
}
