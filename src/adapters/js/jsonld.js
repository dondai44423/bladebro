// Fallback structured extractor using schema.org JSON-LD (<script type="application/ld+json">).
// Invoked ONLY when no site-specific adapter and no repeated list matched.
function extractJsonLd() {
  const scripts = [...document.querySelectorAll('script[type="application/ld+json"]')];
  if (!scripts.length) return null;
  const entities = [];
  const collect = node => {
    if (!node || typeof node !== 'object') return;
    if (Array.isArray(node)) {
      for (const n of node) collect(n);
      return;
    }
    if (Array.isArray(node['@graph'])) collect(node['@graph']);
    if (node['@type']) entities.push(node);
  };
  for (const s of scripts.slice(0, 12)) {
    try {
      const parsed = JSON.parse(s.textContent || '');
      collect(parsed);
    } catch (e) {}
  }
  const hasType = (e, names) => {
    const t = e['@type'];
    const arr = Array.isArray(t) ? t : [t];
    return arr.some(x => typeof x === 'string' && names.includes(x.replace(/^https?:\/\/schema\.org\//i, '')));
  };
  const str = v => (typeof v === 'string' ? v.replace(/\s+/g, ' ').trim() : '');
  const nameOf = v => {
    if (!v) return '';
    if (typeof v === 'string') return str(v);
    if (Array.isArray(v)) return v.map(nameOf).filter(Boolean).join(', ');
    return str(v.name || v.title || '');
  };

  // Priority order: JobPosting > Recipe > Event > Product > SoftwareApplication > Article.
  const job = entities.find(e => hasType(e, ['JobPosting']));
  if (job && str(job.title)) {
    const o = { title: str(job.title).slice(0, 200), url: str(job.url) || location.href.split('#')[0] };
    const co = nameOf(job.hiringOrganization); if (co) o.company = co.slice(0, 120);
    const loc = job.jobLocation && (Array.isArray(job.jobLocation) ? job.jobLocation[0] : job.jobLocation);
    const addr = loc && loc.address;
    if (addr) {
      const parts = [str(addr.addressLocality), str(addr.addressRegion), str(addr.addressCountry)].filter(Boolean);
      if (parts.length) o.location = parts.join(', ');
    }
    if (str(job.jobLocationType)) o.location_type = str(job.jobLocationType);
    if (str(job.employmentType)) o.employment_type = str(job.employmentType);
    if (str(job.datePosted)) o.date_posted = str(job.datePosted);
    if (str(job.validThrough)) o.valid_through = str(job.validThrough);
    if (job.baseSalary && job.baseSalary.value) {
      const sv = job.baseSalary.value;
      const cur = str(job.baseSalary.currency);
      if (sv.minValue || sv.maxValue) o.salary = [cur, sv.minValue, sv.maxValue ? '-' + sv.maxValue : '', str(sv.unitText)].filter(Boolean).join(' ');
      else if (sv.value) o.salary = [cur, sv.value, str(sv.unitText)].filter(Boolean).join(' ');
    }
    const desc = str(job.description).replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
    if (desc) o.description = desc.slice(0, 1200);
    return { container: 'schema-job', detail: true, count: 1, items: [o] };
  }

  const recipe = entities.find(e => hasType(e, ['Recipe']));
  if (recipe && str(recipe.name)) {
    const o = { title: str(recipe.name).slice(0, 200), url: str(recipe.url) || location.href.split('#')[0] };
    const au = nameOf(recipe.author); if (au) o.author = au;
    if (str(recipe.prepTime)) o.prep_time = str(recipe.prepTime);
    if (str(recipe.cookTime)) o.cook_time = str(recipe.cookTime);
    if (str(recipe.totalTime)) o.total_time = str(recipe.totalTime);
    if (recipe.recipeYield) o.yield = nameOf(recipe.recipeYield);
    if (Array.isArray(recipe.recipeIngredient)) o.ingredients = recipe.recipeIngredient.map(str).filter(Boolean).slice(0, 40);
    if (recipe.aggregateRating && recipe.aggregateRating.ratingValue !== undefined) {
      const rv = parseFloat(recipe.aggregateRating.ratingValue);
      if (!isNaN(rv)) o.rating = rv;
    }
    return { container: 'schema-recipe', detail: true, count: 1, items: [o] };
  }

  const ev = entities.find(e => hasType(e, ['Event']));
  if (ev && str(ev.name)) {
    const o = { title: str(ev.name).slice(0, 200), url: str(ev.url) || location.href.split('#')[0] };
    if (str(ev.startDate)) o.start_date = str(ev.startDate);
    if (str(ev.endDate)) o.end_date = str(ev.endDate);
    const loc = nameOf(ev.location); if (loc) o.location = loc;
    const desc = str(ev.description); if (desc) o.description = desc.slice(0, 600);
    return { container: 'schema-event', detail: true, count: 1, items: [o] };
  }

  const prod = entities.find(e => hasType(e, ['Product']));
  if (prod && str(prod.name)) {
    const o = { title: str(prod.name).slice(0, 200), url: str(prod.url) || location.href.split('#')[0] };
    const brand = nameOf(prod.brand); if (brand) o.brand = brand;
    if (str(prod.sku)) o.sku = str(prod.sku);
    const offers = Array.isArray(prod.offers) ? prod.offers[0] : prod.offers;
    if (offers && offers.price !== undefined) {
      const cur = str(offers.priceCurrency);
      o.price = (cur ? cur + ' ' : '') + String(offers.price);
      if (str(offers.availability)) o.availability = str(offers.availability).replace(/^https?:\/\/schema\.org\//i, '');
    }
    if (prod.aggregateRating && prod.aggregateRating.ratingValue !== undefined) {
      const rv = parseFloat(prod.aggregateRating.ratingValue);
      if (!isNaN(rv)) o.rating = rv;
      const rc = prod.aggregateRating.reviewCount ?? prod.aggregateRating.ratingCount;
      if (rc !== undefined) {
        const rcn = parseInt(String(rc), 10);
        if (!isNaN(rcn)) o.reviews = rcn;
      }
    }
    const desc = str(prod.description); if (desc) o.description = desc.slice(0, 600);
    return { container: 'schema-product', detail: true, count: 1, items: [o] };
  }

  const art = entities.find(e => hasType(e, ['NewsArticle', 'Article', 'BlogPosting', 'TechArticle', 'ScholarlyArticle']));
  if (art && (str(art.headline) || str(art.name))) {
    const o = {
      title: (str(art.headline) || str(art.name)).slice(0, 240),
      url: str(art.url) || location.href.split('#')[0],
    };
    const au = nameOf(art.author); if (au) o.author = au.slice(0, 140);
    const pub = nameOf(art.publisher); if (pub) o.publisher = pub.slice(0, 120);
    if (str(art.datePublished)) o.published = str(art.datePublished);
    if (str(art.dateModified) && str(art.dateModified) !== o.published) o.modified = str(art.dateModified);
    const desc = str(art.description); if (desc) o.description = desc.slice(0, 600);
    return { container: 'schema-article', detail: true, count: 1, items: [o] };
  }

  return null;
}
