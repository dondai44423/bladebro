// Developer & research registries: arXiv, PyPI, npm, and crates.io.

// 1. arXiv (/abs/<id>, /search, /list).
if (hostMatches('arxiv.org')) {
  if (location.pathname.startsWith('/abs/')) {
    const titleEl = document.querySelector('h1.title');
    if (titleEl && vtxt(titleEl)) {
      const paper = {
        id: location.pathname.replace(/^\/abs\//, '').replace(/\/$/, ''),
        title: vtxt(titleEl).replace(/^Title:\s*/i, '').trim().slice(0, 300),
        url: location.href.split('#')[0],
      };
      const authors = [...document.querySelectorAll('.authors a')].map(a => vtxt(a)).filter(Boolean);
      if (authors.length) paper.authors = authors.slice(0, 30);
      const dt = document.querySelector('.dateline');
      if (dt && vtxt(dt)) paper.date = vtxt(dt).replace(/^\[|\]$/g, '').trim();
      const subj = document.querySelector('.tablecell.subjects');
      if (subj && vtxt(subj)) paper.subjects = vtxt(subj).slice(0, 200);
      const abs = document.querySelector('blockquote.abstract');
      if (abs && vtxt(abs)) paper.abstract = vtxt(abs).replace(/^Abstract:\s*/i, '').trim().slice(0, 3000);
      const pdf = document.querySelector('a.download-pdf, a[href*="/pdf/"]');
      if (pdf && pdf.href) paper.pdf_url = pdf.href;
      return JSON.stringify({ container: 'arxiv-paper', detail: true, count: 1, items: [paper] });
    }
  }
  const searchRows = [...document.querySelectorAll('li.arxiv-result')];
  if (searchRows.length >= 1) {
    const items = searchRows.slice(0, __LIMIT__).map(r => {
      const o = {};
      const idLink = r.querySelector('.list-title a[href*="/abs/"]');
      if (idLink) {
        o.id = vtxt(idLink).replace(/^arXiv:/i, '').trim();
        o.url = absURL(idLink.getAttribute('href') || '');
      }
      const titleEl = r.querySelector('p.title');
      if (titleEl && vtxt(titleEl)) o.title = vtxt(titleEl).slice(0, 240);
      const authors = [...r.querySelectorAll('.authors a')].map(a => vtxt(a)).filter(Boolean);
      if (authors.length) o.authors = authors.slice(0, 12);
      const absEl = r.querySelector('.abstract-full, .abstract-short');
      if (absEl && vtxt(absEl)) o.abstract = vtxt(absEl).replace(/\s*(?:▽\s*More|△\s*Less)\s*$/g, '').trim().slice(0, 500);
      const pdf = r.querySelector('a[href*="/pdf/"]');
      if (pdf && pdf.href) o.pdf_url = pdf.href;
      return o;
    }).filter(o => o.title && o.url);
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('query') || '';
      const out = { container: 'arxiv-papers', count: items.length, items };
      if (q) out.query = q;
      return JSON.stringify(out);
    }
  }
  // /list/... pages use <dl><dt>...</dt><dd>...</dd></dl>.
  if (location.pathname.startsWith('/list/')) {
    const dts = [...document.querySelectorAll('#articles dt, dl dt')];
    if (dts.length >= 1) {
      const items = [];
      for (const dt of dts) {
        if (items.length >= __LIMIT__) break;
        const dd = dt.nextElementSibling;
        if (!dd || dd.tagName !== 'DD') continue;
        const idLink = dt.querySelector('a[href*="/abs/"]');
        const titleEl = dd.querySelector('.list-title');
        if (!idLink || !titleEl) continue;
        const o = {
          id: vtxt(idLink).replace(/^arXiv:/i, '').trim(),
          title: vtxt(titleEl).replace(/^Title:\s*/i, '').trim().slice(0, 240),
          url: absURL(idLink.getAttribute('href') || ''),
        };
        const authors = [...dd.querySelectorAll('.list-authors a')].map(a => vtxt(a)).filter(Boolean);
        if (authors.length) o.authors = authors.slice(0, 12);
        const subj = dd.querySelector('.list-subjects');
        if (subj && vtxt(subj)) o.subjects = vtxt(subj).replace(/^Subjects:\s*/i, '').trim().slice(0, 160);
        const pdf = dt.querySelector('a[href*="/pdf/"]');
        if (pdf && pdf.href) o.pdf_url = pdf.href;
        items.push(o);
      }
      if (items.length > 0) {
        return JSON.stringify({ container: 'arxiv-papers', count: items.length, items });
      }
    }
  }
}

// 2. PyPI (/project/<name>, /search).
if (hostMatches('pypi.org')) {
  if (location.pathname.startsWith('/project/')) {
    const h1 = document.querySelector('h1.project-header__name, h1.package-header__name');
    if (h1 && vtxt(h1)) {
      const parts = vtxt(h1).split(/\s+/);
      const pkg = {
        name: parts[0],
        url: location.href.split('#')[0],
      };
      if (parts[1]) pkg.version = parts[1];
      const sum = document.querySelector('.package-description__summary, .project-description__summary');
      const sumTxt = (sum && vtxt(sum)) || (document.querySelector('meta[name="description"]')?.content || '').trim();
      if (sumTxt) pkg.summary = sumTxt.slice(0, 400);
      const pip = document.querySelector('#pip-command, .project-header__pip-instructions code');
      if (pip && vtxt(pip)) pkg.install = vtxt(pip);
      const dt = document.querySelector('.package-header__date time, .project-header__date time, time[datetime]');
      if (dt) pkg.released = dt.getAttribute('datetime') || vtxt(dt);
      for (const p of document.querySelectorAll('.sidebar-section p')) {
        const t = vtxt(p);
        if (/^Author:/i.test(t) && !pkg.author) pkg.author = t.replace(/^Author:\s*/i, '').trim().slice(0, 120);
        else if (/^License:/i.test(t) && !pkg.license) pkg.license = t.replace(/^License:\s*/i, '').trim().slice(0, 120);
        else if (/^Requires:\s*Python/i.test(t) && !pkg.requires_python) pkg.requires_python = t.replace(/^Requires:\s*/i, '').trim();
      }
      return JSON.stringify({ container: 'pypi-package', detail: true, count: 1, items: [pkg] });
    }
  }
  const pyRows = [...document.querySelectorAll('a.package-snippet')];
  if (pyRows.length >= 1) {
    const items = pyRows.slice(0, __LIMIT__).map(a => {
      const o = { url: absURL(a.getAttribute('href') || '') };
      const nm = a.querySelector('.package-snippet__name');
      if (nm && vtxt(nm)) o.name = vtxt(nm);
      const vr = a.querySelector('.package-snippet__version');
      if (vr && vtxt(vr)) o.version = vtxt(vr);
      const dt = a.querySelector('.package-snippet__created time');
      if (dt) o.released = dt.getAttribute('datetime') || vtxt(dt);
      const ds = a.querySelector('.package-snippet__description');
      if (ds && vtxt(ds)) o.description = vtxt(ds).slice(0, 300);
      return o;
    }).filter(o => o.name && o.url);
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('q') || '';
      const out = { container: 'pypi-search', count: items.length, items };
      if (q) out.query = q;
      return JSON.stringify(out);
    }
  }
}

// 3. npm (/package/<name>, /search?q=...) — guarded so Cloudflare challenge pages never false-match.
if (hostMatches('npmjs.com')) {
  if (location.pathname.startsWith('/package/')) {
    const h1 = document.querySelector('#top h1, main h1');
    const name = h1 ? vtxt(h1) : '';
    const installEl = document.querySelector('code[title*="npm i"], p code');
    if (name && name !== 'www.npmjs.com' && (installEl || document.querySelector('#readme'))) {
      const pkg = { name: name.slice(0, 120), url: location.href.split('#')[0] };
      const topTxt = vtxt(document.querySelector('#top') || document.querySelector('main'));
      const vm = topTxt.match(/\b(\d+\.\d+\.\d+[\w.-]*)\s*•/);
      if (vm) pkg.version = vm[1];
      if (installEl && vtxt(installEl)) pkg.install = vtxt(installEl);
      const descMeta = (document.querySelector('meta[name="description"]')?.content || '').trim();
      if (descMeta) pkg.description = descMeta.slice(0, 300);
      const dl = topTxt.match(/Weekly Downloads\s*([\d,]+)/i);
      if (dl) pkg.weekly_downloads = parseInt(dl[1].replace(/,/g, ''), 10);
      const lic = topTxt.match(/License\s*([A-Za-z0-9._+-]+)/i);
      if (lic) pkg.license = lic[1];
      return JSON.stringify({ container: 'npm-package', detail: true, count: 1, items: [pkg] });
    }
  }
  if (location.pathname === '/search') {
    const secs = [...document.querySelectorAll('main section')].filter(s => s.querySelector('a[href*="/package/"]'));
    if (secs.length >= 1) {
      const items = secs.slice(0, __LIMIT__).map(s => {
        const a = s.querySelector('a[href*="/package/"]');
        const o = {
          name: a ? vtxt(a) : '',
          url: a ? absURL(a.getAttribute('href') || '') : '',
        };
        const p = s.querySelector('p');
        if (p && vtxt(p)) o.description = vtxt(p).slice(0, 300);
        const ft = vtxt(s);
        const vm = ft.match(/published\s+([^\s•]+)\s*•\s*([^•\n]+)/i);
        if (vm) {
          o.version = vm[1];
          o.updated = vm[2].trim();
        }
        return o;
      }).filter(o => o.name && o.url);
      if (items.length > 0) {
        const q = new URLSearchParams(location.search).get('q') || '';
        const out = { container: 'npm-search', count: items.length, items };
        if (q) out.query = q;
        return JSON.stringify(out);
      }
    }
  }
}

// 4. crates.io (/crates/<name>, /search?q=...) — uses semantic HTML (data-test-* is stripped in prod).
if (hostMatches('crates.io')) {
  const cParts = location.pathname.split('/').filter(Boolean);
  if (cParts[0] === 'crates' && cParts.length >= 2) {
    const main = document.querySelector('main');
    if (main && vtxt(main).length > 40) {
      const h = [...main.querySelectorAll('h1, h2')].find(el => {
        const t = vtxt(el);
        return t && t.toLowerCase().startsWith(cParts[1].toLowerCase());
      });
      const pkg = {
        name: cParts[1],
        url: location.origin + '/crates/' + cParts[1],
      };
      if (h) {
        const vm = vtxt(h).match(/\bv(\d+\.\d+\.\d+[\w.-]*)/i);
        if (vm) pkg.version = vm[1];
      }
      const mt = vtxt(main);
      if (!pkg.version) {
        const vm = mt.match(/\bv(\d+\.\d+\.\d+[\w.-]*)/);
        if (vm) pkg.version = vm[1];
      }
      const descEl = main.querySelector('[class*="description"], p');
      if (descEl && vtxt(descEl) && vtxt(descEl).length > 5 && vtxt(descEl).length < 400) {
        pkg.description = vtxt(descEl).slice(0, 300);
      }
      pkg.install = 'cargo add ' + cParts[1];
      const dlm = mt.match(/All-Time\s*([\d,]+)/i) || mt.match(/Downloads\s*([\d,]+)/i);
      if (dlm) pkg.downloads = parseInt(dlm[1].replace(/,/g, ''), 10);
      const lic = mt.match(/License\s*([^\n]{2,60})/i);
      if (lic) pkg.license = lic[1].trim();
      const repo = main.querySelector('a[href^="https://github.com/"], a[href^="https://gitlab.com/"], a[href^="https://codeberg.org/"]');
      if (repo && repo.href) pkg.repository = repo.href;
      const docs = main.querySelector('a[href^="https://docs.rs/"]');
      if (docs && docs.href) pkg.documentation = docs.href;
      return JSON.stringify({ container: 'crates-package', detail: true, count: 1, items: [pkg] });
    }
  }
  if (cParts[0] === 'search') {
    const rows = [...document.querySelectorAll('main ol li')].filter(li => li.querySelector('a[href^="/crates/"]'));
    if (rows.length >= 1) {
      const items = rows.slice(0, __LIMIT__).map(li => {
        const a = li.querySelector('a[href^="/crates/"]');
        const o = {
          name: a ? vtxt(a) : '',
          url: a ? absURL(a.getAttribute('href') || '') : '',
        };
        const ft = vtxt(li);
        const vm = ft.match(/\bv(\d+\.\d+\.\d+[\w.-]*)/);
        if (vm) o.version = vm[1];
        const dlm = ft.match(/All-Time:\s*([\d,]+)/i) || ft.match(/([\d,]+)\s+downloads/i);
        if (dlm) o.downloads = parseInt(dlm[1].replace(/,/g, ''), 10);
        const desc = [...li.querySelectorAll('div, p')].map(e => vtxt(e)).find(t => t.length > 15 && t.length < 300 && !t.includes(o.name));
        if (desc) o.description = desc.slice(0, 280);
        return o;
      }).filter(o => o.name && o.url);
      if (items.length > 0) {
        const q = new URLSearchParams(location.search).get('q') || '';
        const out = { container: 'crates-search', count: items.length, items };
        if (q) out.query = q;
        return JSON.stringify(out);
      }
    }
  }
}
