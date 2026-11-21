// Reddit post & comment-tree HTML/DOM parser (Shreddit + old.reddit.com).
// Pure & observational: works identically on live `document` and detached
// `DOMParser` HTML fragments without mutating the page or calling JSON APIs.
function parseRedditThread(root, opts) {
  opts = opts || {};
  const isFragment = !!opts.isFragment;
  const fallbackParent = opts.fallbackParent || '';
  const origin = opts.origin || (typeof location !== 'undefined' && location.origin) || 'https://www.reddit.com';
  const pageHref = opts.url || (typeof location !== 'undefined' && location.href) || (origin + '/');
  const pagePath = opts.pathname || (typeof location !== 'undefined' && location.pathname) || '/';

  function stripPrefix(s) {
    if (!s) return '';
    return String(s).replace(/^t[13]_/i, '').trim();
  }

  function cleanAuthor(raw) {
    const a = String(raw || '').trim().replace(/^\/?u\//i, '');
    if (!a || /^\[?(deleted|removed)\]?$/i.test(a)) return '[deleted]';
    return a;
  }

  function parseIntSafe(v) {
    if (v === null || v === undefined || v === '') return null;
    const s = String(v).trim().replace(/,/g, '');
    const m = s.match(/^(-?\d+(?:\.\d+)?)\s*([kKmMbB])?/);
    if (!m) return null;
    let n = parseFloat(m[1]);
    if (!isFinite(n)) return null;
    const suf = (m[2] || '').toLowerCase();
    if (suf === 'k') n *= 1000;
    else if (suf === 'm') n *= 1000000;
    else if (suf === 'b') n *= 1000000000;
    return Math.round(n);
  }

  function toAbsUrl(href) {
    if (!href) return '';
    const h = String(href).trim();
    if (!h || h.charAt(0) === '#' || /^javascript:/i.test(h)) return '';
    try {
      return new URL(h, pageHref).href;
    } catch (e) {
      return '';
    }
  }

  function toSameOriginPath(href) {
    if (!href) return '';
    const h = String(href).trim();
    if (!h || h.charAt(0) === '#' || /^javascript:/i.test(h)) return '';
    try {
      const u = new URL(h, pageHref);
      const base = new URL(origin);
      if (u.origin !== base.origin) return '';
      return u.pathname + u.search;
    } catch (e) {
      return h.charAt(0) === '/' ? h : '';
    }
  }

  function idFromCommentUrl(href) {
    if (!href) return '';
    const m = String(href).match(/\/comment\/([a-z0-9]+)/i)
      || String(href).match(/\/comments\/[a-z0-9]+\/[^/]*\/([a-z0-9]+)(?:\/|\?|$)/i);
    return m ? m[1] : '';
  }

  function postIdFromPath(path) {
    const m = String(path || '').match(/\/comments\/([a-z0-9]+)/i);
    return m ? m[1] : '';
  }

  function subFromPath(path) {
    const m = String(path || '').match(/\/r\/([A-Za-z0-9_+-]+)/);
    return m ? ('r/' + m[1]) : '';
  }

  // Deterministic text extractor that preserves paragraph/list/code breaks in
  // both live DOM and detached DOMParser documents (where innerText is empty).
  const SKIP_TEXT = { SCRIPT: 1, STYLE: 1, NOSCRIPT: 1, SVG: 1, TEMPLATE: 1, BUTTON: 1 };
  const BLOCK_TAGS = { P: 1, DIV: 1, LI: 1, BLOCKQUOTE: 1, PRE: 1, H1: 1, H2: 1, H3: 1, H4: 1, H5: 1, H6: 1, TR: 1 };
  function extractRichText(el, ownerComment) {
    if (!el) return '';
    const parts = [];
    let nodes = 0;
    function walk(n) {
      if (++nodes > 12000) return;
      if (n.nodeType === 3) {
        const t = n.textContent || '';
        if (t) parts.push(t);
        return;
      }
      if (n.nodeType !== 1) return;
      const tag = n.tagName || '';
      if (SKIP_TEXT[tag]) return;
      if (n.hasAttribute && n.hasAttribute('hidden')) return;
      if (ownerComment && n !== ownerComment) {
        if (tag === 'SHREDDIT-COMMENT') return;
        if (n.classList && n.classList.contains('comment') && n.classList.contains('thing')) return;
      }
      if (tag === 'BR') {
        parts.push('\n');
        return;
      }
      const isBlock = !!BLOCK_TAGS[tag];
      if (isBlock && parts.length > 0) parts.push('\n');
      if (tag === 'LI') parts.push('- ');
      if (tag === 'BLOCKQUOTE') parts.push('> ');
      const kids = n.childNodes || [];
      for (let i = 0; i < kids.length; i++) walk(kids[i]);
      if (isBlock) parts.push('\n');
    }
    walk(el);
    return parts
      .join('')
      .replace(/[ \t\f\v]+/g, ' ')
      .replace(/ *\n */g, '\n')
      .replace(/\n{3,}/g, '\n\n')
      .trim();
  }

  // Plain label extractor for expansion controls (includes <button> text).
  const SKIP_LABEL = { SCRIPT: 1, STYLE: 1, NOSCRIPT: 1, SVG: 1, TEMPLATE: 1 };
  function extractLabelText(el) {
    if (!el) return '';
    const parts = [];
    let nodes = 0;
    function walk(n) {
      if (++nodes > 2000) return;
      if (n.nodeType === 3) {
        if (n.textContent) parts.push(n.textContent);
        return;
      }
      if (n.nodeType !== 1) return;
      const tag = n.tagName || '';
      if (SKIP_LABEL[tag]) return;
      if (n.hasAttribute && n.hasAttribute('hidden')) return;
      const kids = n.childNodes || [];
      for (let i = 0; i < kids.length; i++) walk(kids[i]);
    }
    walk(el);
    return parts.join(' ').replace(/\s+/g, ' ').trim();
  }

  function isOwnedByComment(el, commentEl, tagOrSelector) {
    if (!el || !commentEl) return false;
    if (typeof el.closest === 'function') {
      const nearest = el.closest(tagOrSelector);
      return nearest === commentEl;
    }
    let cur = el.parentElement;
    while (cur) {
      if (cur === commentEl) return true;
      if (tagOrSelector === 'shreddit-comment' && cur.tagName === 'SHREDDIT-COMMENT') return false;
      if (tagOrSelector === '.thing.comment' && cur.classList && cur.classList.contains('thing') && cur.classList.contains('comment')) return false;
      cur = cur.parentElement;
    }
    return false;
  }

  function firstOwned(commentEl, selector, ownerSel) {
    const list = commentEl.querySelectorAll(selector);
    for (let i = 0; i < list.length; i++) {
      if (isOwnedByComment(list[i], commentEl, ownerSel)) return list[i];
    }
    return null;
  }

  // ── 1. Post metadata (full page only) ───────────────────────────────
  let post = null;
  let postId = postIdFromPath(pagePath);
  if (!isFragment) {
    const sp = root.querySelector('shreddit-post');
    if (sp) {
      const a = n => sp.getAttribute(n) || '';
      const id = stripPrefix(a('id')) || postIdFromPath(a('permalink')) || postId;
      if (id) postId = id;
      const permalink = a('permalink') || pagePath;
      const subRaw = a('subreddit-prefixed-name') || a('subreddit-name') || subFromPath(permalink);
      const subreddit = subRaw ? (subRaw.indexOf('/') >= 0 ? subRaw : 'r/' + subRaw) : '';
      const h1 = root.querySelector('h1');
      const title = (a('post-title') || (h1 ? extractRichText(h1) : '') || (root.title || '')).trim();
      const tsEl = sp.querySelector('faceplate-timeago[ts], time[datetime]');
      const date = a('created-timestamp')
        || (tsEl ? (tsEl.getAttribute('ts') || tsEl.getAttribute('datetime') || '') : '');
      const bodyEl = sp.querySelector('[slot="text-body"], [id$="-post-rtjson-content"], [data-click-id="text"] .md, .md');
      let body = '';
      if (bodyEl && (!bodyEl.closest || !bodyEl.closest('shreddit-comment'))) {
        body = extractRichText(bodyEl);
      }
      post = {
        id: postId,
        title,
        author: cleanAuthor(a('author')),
        subreddit,
        score: parseIntSafe(a('score')),
        comments: parseIntSafe(a('comment-count')),
        date: date || null,
        permalink,
        url: toAbsUrl(permalink) || pageHref,
        body: body || null,
      };
    } else {
      // old.reddit.com post (.thing.link inside #siteTable).
      const oldPost = root.querySelector('#siteTable .thing.link, .thing.link');
      if (oldPost) {
        const a = n => oldPost.getAttribute(n) || '';
        const id = stripPrefix(a('data-fullname')) || postIdFromPath(a('data-permalink')) || postId;
        if (id) postId = id;
        const permalink = a('data-permalink') || pagePath;
        const subRaw = a('data-subreddit') || subFromPath(permalink);
        const subreddit = subRaw ? (subRaw.indexOf('/') >= 0 ? subRaw : 'r/' + subRaw) : '';
        const tl = oldPost.querySelector('a.title');
        const title = (tl ? extractRichText(tl) : (root.title || '')).trim();
        const tm = oldPost.querySelector('time[datetime]');
        const bodyEl = oldPost.querySelector('.expando .usertext-body .md, .usertext-body .md');
        const body = bodyEl ? extractRichText(bodyEl) : '';
        post = {
          id: postId,
          title,
          author: cleanAuthor(a('data-author')),
          subreddit,
          score: parseIntSafe(a('data-score')),
          comments: parseIntSafe(a('data-comments-count')),
          date: tm ? (tm.getAttribute('datetime') || null) : null,
          permalink,
          url: toAbsUrl(permalink) || pageHref,
          body: body || null,
        };
      }
    }
  }

  // ── 2. Comments (Shreddit + old.reddit.com) ─────────────────────────
  const comments = [];
  const shComments = [...root.querySelectorAll('shreddit-comment')];
  if (shComments.length > 0) {
    for (let i = 0; i < shComments.length; i++) {
      const sc = shComments[i];
      const a = n => sc.getAttribute(n) || '';
      const ownLink = firstOwned(sc, 'a[href*="/comment/"]', 'shreddit-comment');
      const permalink = a('permalink') || (ownLink ? toSameOriginPath(ownLink.getAttribute('href')) : '');
      const id = stripPrefix(a('thingid') || a('id')) || idFromCommentUrl(permalink);
      if (!id) continue;

      let parentId = stripPrefix(a('parentid'));
      if (!parentId && sc.parentElement && typeof sc.parentElement.closest === 'function') {
        const anc = sc.parentElement.closest('shreddit-comment');
        if (anc) parentId = stripPrefix(anc.getAttribute('thingid') || anc.getAttribute('id'));
      }
      if (!parentId) parentId = fallbackParent || postId;

      const isDeleted = sc.hasAttribute('is-comment-deleted');
      const isRemoved = sc.hasAttribute('is-comment-removed');
      const authorRaw = sc.hasAttribute('is-author-deleted') ? '[deleted]' : a('author');
      const author = cleanAuthor(authorRaw);
      const score = parseIntSafe(a('score'));
      const depth = parseIntSafe(a('depth'));

      let date = a('created') || a('created-timestamp') || '';
      if (!date) {
        const ta = firstOwned(sc, 'faceplate-timeago[ts], time[datetime]', 'shreddit-comment');
        if (ta) date = ta.getAttribute('ts') || ta.getAttribute('datetime') || '';
      }

      const bodyEl = firstOwned(
        sc,
        '[slot="comment"], [id$="-comment-rtjson-content"], [data-testid="comment"], .md',
        'shreddit-comment'
      );
      let body = bodyEl ? extractRichText(bodyEl, sc) : '';
      if (!body) {
        if (isRemoved) body = '[removed]';
        else if (isDeleted) body = '[deleted]';
      }

      comments.push({
        id,
        parent_id: parentId,
        author,
        score,
        depth,
        date: date || null,
        permalink,
        body,
      });
    }
  } else {
    // old.reddit.com comments (.thing.comment[data-fullname^="t1_"]).
    const oldComments = [...root.querySelectorAll('.thing.comment')];
    for (let i = 0; i < oldComments.length; i++) {
      const oc = oldComments[i];
      const a = n => oc.getAttribute(n) || '';
      const full = a('data-fullname');
      const id = stripPrefix(full);
      if (!id) continue;
      let parentId = '';
      if (oc.parentElement && typeof oc.parentElement.closest === 'function') {
        const anc = oc.parentElement.closest('.thing.comment');
        if (anc) parentId = stripPrefix(anc.getAttribute('data-fullname'));
      }
      if (!parentId) parentId = fallbackParent || postId;

      const auEl = firstOwned(oc, '.entry .tagline .author', '.thing.comment');
      const author = cleanAuthor(a('data-author') || (auEl ? extractRichText(auEl) : ''));
      const scEl = firstOwned(oc, '.entry .score.unvoted', '.thing.comment');
      const score = scEl ? parseIntSafe(scEl.getAttribute('title') || extractRichText(scEl)) : null;
      const tmEl = firstOwned(oc, '.entry time[datetime]', '.thing.comment');
      const date = tmEl ? (tmEl.getAttribute('datetime') || null) : null;
      const linkEl = firstOwned(oc, '.entry a.bylink, .entry a[data-event-action="permalink"]', '.thing.comment');
      const permalink = a('data-permalink') || (linkEl ? toSameOriginPath(linkEl.getAttribute('href')) : '');
      const bodyEl = firstOwned(oc, '.entry .usertext-body .md', '.thing.comment');
      const body = bodyEl ? extractRichText(bodyEl, oc) : '';

      comments.push({
        id,
        parent_id: parentId,
        author,
        score,
        depth: null,
        date,
        permalink,
        body,
      });
    }
  }

  // ── 3. Expansion controls: <faceplate-partial> & deep fold-more links ─
  const NON_COMMENT_PARTIAL = /\/(?:composer|award|share|report|vote|mod|overflow|profile|avatar|hovercard|flair|sidebar|related|recommend|pdp-right-rail|right-sidebar|nav|inbox|notification|ad[s]?)\b/i;
  const partials = [];
  const partialSeen = new Set();
  let clickableOnlyPartials = 0;

  const partialScope = isFragment
    ? root
    : (root.querySelector('main, [role="main"]') || root);
  const fpList = [...partialScope.querySelectorAll('faceplate-partial')];
  for (let i = 0; i < fpList.length; i++) {
    const fp = fpList[i];
    if (!isFragment && typeof fp.closest === 'function' && fp.closest('aside, nav, header, footer, [slot="right-sidebar"]')) {
      continue;
    }
    const method = (fp.getAttribute('method') || '').trim().toLowerCase();
    if (method && method !== 'get') continue;

    const rawSrc = fp.getAttribute('src') || '';
    const nameId = ((fp.getAttribute('name') || '') + ' ' + (fp.getAttribute('id') || '')).toLowerCase();
    const label = extractLabelText(fp).slice(0, 120);
    const inTree = isFragment || (typeof fp.closest === 'function' && !!fp.closest('shreddit-comment-tree, shreddit-comment, #comment-tree'));
    const looksLikeMoreLabel = /\b(?:more\s+repl|more\s+comment|view\s+more|load\s+more|show\s+more)/i.test(label);

    if (!rawSrc) {
      if (inTree && (looksLikeMoreLabel || nameId.indexOf('more-comments') >= 0)) {
        clickableOnlyPartials++;
      }
      continue;
    }
    const path = toSameOriginPath(rawSrc);
    if (!path || NON_COMMENT_PARTIAL.test(path)) continue;

    const isSvcComment = /\/svc\/shreddit\/(?:more-comments|comments)\//i.test(path);
    const isNamedMore = inTree && (nameId.indexOf('more-comments') >= 0 || nameId.indexOf('comment-tree') >= 0 || looksLikeMoreLabel);
    if (!isSvcComment && !isNamedMore) continue;
    if (partialSeen.has(path)) continue;
    partialSeen.add(path);

    let parentId = fallbackParent || postId;
    if (typeof fp.closest === 'function') {
      const anc = fp.closest('shreddit-comment');
      if (anc) parentId = stripPrefix(anc.getAttribute('thingid') || anc.getAttribute('id')) || parentId;
    }
    const cm = label.match(/(\d[\d,]*)\s+more/i);
    const claimed = cm ? parseIntSafe(cm[1]) : null;
    partials.push({
      url: path,
      parent_id: parentId,
      label,
      claimed,
    });
  }

  // Deep-thread continuation links: Shreddit `div.fold-more a` /
  // `force-legacy-sct` links, and old.reddit `.deepthread a`.
  // Hidden SEO `a.more-comments-link` elements are intentionally skipped.
  const folds = [];
  const foldSeen = new Set();
  const foldCandidates = [
    ...partialScope.querySelectorAll('div.fold-more a[href], a[href*="force-legacy-sct"], .deepthread a[href], shreddit-comment a[href*="/comment/"]'),
  ];
  for (let i = 0; i < foldCandidates.length; i++) {
    const aEl = foldCandidates[i];
    if (aEl.classList && aEl.classList.contains('more-comments-link')) continue;
    if (aEl.hasAttribute && aEl.hasAttribute('hidden')) continue;
    const rawHref = aEl.getAttribute('href') || '';
    const path = toSameOriginPath(rawHref);
    if (!path || (path.indexOf('/comment/') < 0 && path.indexOf('/comments/') < 0)) continue;

    const inFold = typeof aEl.closest === 'function' && !!aEl.closest('div.fold-more, .deepthread');
    const isLegacySct = path.indexOf('force-legacy-sct') >= 0;
    const label = extractLabelText(aEl).slice(0, 120);
    const looksLikeContinue = /\b(?:more\s+repl|continue\s+this\s+thread|view\s+more\s+repl)/i.test(label);
    if (!inFold && !isLegacySct && !looksLikeContinue) continue;

    const cid = idFromCommentUrl(path);
    const dedupeKey = cid || path;
    if (foldSeen.has(dedupeKey)) continue;
    foldSeen.add(dedupeKey);

    let parentId = fallbackParent || postId;
    if (typeof aEl.closest === 'function') {
      const anc = aEl.closest('shreddit-comment') || aEl.closest('.thing.comment');
      if (anc) {
        parentId = stripPrefix(anc.getAttribute('thingid') || anc.getAttribute('data-fullname') || anc.getAttribute('id')) || parentId;
      }
    }
    folds.push({
      url: path,
      parent_id: parentId,
      comment_id: cid,
      label,
    });
  }

  // old.reddit `.morechildren` buttons (require legacy POST /api/morechildren;
  // report honestly if any remain unexpanded).
  const legacyMoreCount = root.querySelectorAll('.thing.morechildren').length;

  return {
    post,
    post_id: postId,
    comments,
    partials,
    folds,
    clickable_only_partials: clickableOnlyPartials,
    legacy_more_count: legacyMoreCount,
  };
}
