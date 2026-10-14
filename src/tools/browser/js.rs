//! Page-side scripts. Placeholders (__X__) are replaced with JSON literals before evaluation.

/// Selector for things a user can interact with.
const INTER: &str = r#"a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=menuitemcheckbox],[role=menuitemradio],[role=option],[role=switch],[role=combobox],[role=textbox],[role=searchbox],[role=slider],[role=treeitem],[contenteditable=""],[contenteditable="true"],[onclick]"#;

const COMMON: &str = r#"
  const INTER = __INTER__;
  const clean = s => (s || '').replace(/\s+/g, ' ').trim();
  const vis = el => el.checkVisibility ? el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }) : !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
  const nameOf = el => {
    let s = el.getAttribute('aria-label') || '';
    const lb = el.getAttribute('aria-labelledby');
    if (!s && lb) s = lb.split(/\s+/).map(id => (document.getElementById(id) || {}).innerText || '').join(' ');
    const t = el.tagName;
    if (!s && el.labels && el.labels.length) s = el.labels[0].innerText;
    if (!s && (t === 'INPUT' || t === 'TEXTAREA')) s = el.placeholder || (['submit', 'button', 'reset'].includes(el.type) ? el.value : '') || el.title || el.name || '';
    if (!s && t !== 'SELECT') s = el.innerText || el.title || el.getAttribute('alt') || ((el.querySelector && el.querySelector('img[alt]')) || {}).alt || '';
    if (!s && t === 'SELECT') s = el.title || el.name || '';
    return clean(s).slice(0, 100);
  };
"#;

const SNAPSHOT: &str = r#"(() => {
  const MAX = __MAX__, FILTER = __FILTER__, ONLY = __ONLY__;
  __COMMON__
  const refs = window.__nrefs = [null];
  const lines = []; let size = 0, more = 0;
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'svg', 'SVG', 'CANVAS', 'VIDEO', 'AUDIO', 'IFRAME', 'OBJECT', 'EMBED']);
  const BLOCK = new Set(['P', 'LI', 'TD', 'TH', 'DT', 'DD', 'BLOCKQUOTE', 'PRE', 'LABEL', 'FIGCAPTION', 'CAPTION', 'LEGEND']);
  const WORDY = /[\p{L}\p{N}]/u;
  const push = s => {
    if (FILTER && !s.toLowerCase().includes(FILTER)) return;
    if (size + s.length > MAX) { more += s.length; return; }
    lines.push(s); size += s.length + 1;
  };
  const short = h => { try { const u = new URL(h, location.href); return (u.origin === location.origin ? u.pathname + u.search + u.hash : u.host + u.pathname).slice(0, 80); } catch (e) { return ''; } };
  const roleOf = el => {
    const r = el.getAttribute('role'); if (r) return r;
    const t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'button' || t === 'summary') return 'button';
    if (t === 'select') return 'select';
    if (t === 'textarea') return 'textbox';
    if (t === 'input') {
      const ty = (el.type || 'text').toLowerCase();
      if (ty === 'checkbox' || ty === 'radio') return ty;
      if (['submit', 'button', 'reset', 'image'].includes(ty)) return 'button';
      if (ty === 'file') return 'file-input';
      return ty === 'text' ? 'textbox' : 'input:' + ty;
    }
    if (el.isContentEditable) return 'editable';
    return t;
  };
  const inter = el => {
    let s = `[${refs.length}] ${roleOf(el)}`; refs.push(el);
    const n = nameOf(el); if (n) s += ` "${n}"`;
    const t = el.tagName;
    if (t === 'INPUT' || t === 'TEXTAREA') {
      const ty = (el.type || '').toLowerCase();
      if (ty === 'checkbox' || ty === 'radio') { if (el.checked) s += ' (checked)'; }
      else if (!['submit', 'button', 'reset', 'image', 'file'].includes(ty)) s += ` = "${ty === 'password' ? (el.value ? '•••' : '') : clean(el.value).slice(0, 80)}"`;
    } else if (t === 'SELECT') {
      const o = el.selectedOptions[0]; s += ` = "${clean(o ? o.text : '')}"`;
      const opts = [...el.options].slice(0, 12).map(o => clean(o.text)).filter(Boolean);
      if (opts.length) s += ` options: ${opts.join(' | ')}${el.options.length > 12 ? ' …' : ''}`;
    } else if (el.isContentEditable && !n) {
      s += ` = "${clean(el.innerText).slice(0, 80)}"`;
    }
    if (['aria-checked', 'aria-pressed', 'aria-selected'].some(a => el.getAttribute(a) === 'true')) s += ' (on)';
    const ex = el.getAttribute('aria-expanded'); if (ex) s += ex === 'true' ? ' (expanded)' : ' (collapsed)';
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') s += ' (disabled)';
    if (t === 'A') { const h = el.getAttribute('href') || ''; if (h && !h.startsWith('javascript') && h !== '#') { const sh = short(el.href); if (sh) s += ` → ${sh}`; } }
    const r = el.getBoundingClientRect();
    if (r.bottom < 0 || r.top > innerHeight) s += ' ⋯';
    push(s);
  };
  const walk = root => {
    for (const el of root.children) {
      if (SKIP.has(el.tagName)) continue;
      if (el.tagName === 'IMG') { if (!ONLY && vis(el)) { const a = clean(el.alt); if (a) push(`img "${a.slice(0, 80)}"`); } continue; }
      if (!vis(el)) {
        if (getComputedStyle(el).display === 'contents') walk(el);
        continue;
      }
      if (el.matches(INTER)) {
        inter(el);
        if (['SELECT', 'INPUT', 'TEXTAREA'].includes(el.tagName) || !el.querySelector(INTER)) continue;
      } else if (/^H[1-6]$/.test(el.tagName)) {
        if (!ONLY) { const t = clean(el.innerText); if (t) push('#'.repeat(+el.tagName[1]) + ' ' + t.slice(0, 200)); }
        if (!el.querySelector(INTER)) continue;
      } else if (!ONLY) {
        if (BLOCK.has(el.tagName) && !el.querySelector(INTER)) { const t = clean(el.innerText); if (t) push(t.slice(0, 300)); continue; }
        let own = ''; for (const c of el.childNodes) if (c.nodeType === 3) own += c.textContent;
        own = clean(own); if (own.length > 1 && WORDY.test(own)) push(own.slice(0, 300));
      }
      if (el.shadowRoot) walk(el.shadowRoot);
      walk(el);
    }
  };
  walk(document.body || document.documentElement);
  return { title: document.title, url: location.href, lines, more, refs: refs.length - 1,
           sx: Math.round(scrollX), sy: Math.round(scrollY), vw: innerWidth, vh: innerHeight,
           dh: Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0) };
})()"#;

/// Resolve {ref|selector|text}, remember it in window.__ncur, scroll it into view and describe it.
const FIND: &str = r#"((spec) => {
  __COMMON__
  let el = null;
  if (spec.ref != null) {
    el = (window.__nrefs || [])[spec.ref];
    if (!el) throw new Error(`ref [${spec.ref}] is unknown here: take a fresh snapshot`);
    if (!el.isConnected) throw new Error(`ref [${spec.ref}] is gone (the page changed): take a fresh snapshot`);
  } else if (spec.selector) {
    const s = spec.selector;
    el = (s.startsWith('/') || s.startsWith('xpath='))
      ? document.evaluate(s.replace(/^xpath=/, ''), document, null, 9, null).singleNodeValue
      : document.querySelector(s);
    if (!el) throw new Error(`nothing matches selector ${s}`);
  } else if (spec.text) {
    const q = clean(spec.text).toLowerCase();
    const cands = [...document.querySelectorAll(INTER)].filter(vis);
    const nm = e => nameOf(e).toLowerCase();
    el = cands.find(e => nm(e) === q) || cands.find(e => nm(e).includes(q));
    if (!el) {
      let best = null, bl = 1e9;
      const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      while (w.nextNode()) {
        const t = clean(w.currentNode.textContent).toLowerCase();
        if (t && t.includes(q) && t.length < bl) { const p = w.currentNode.parentElement; if (p && vis(p)) { best = p; bl = t.length; } }
      }
      el = best;
    }
    if (!el) throw new Error(`nothing visible matches text "${spec.text}"`);
  } else if (document.activeElement && document.activeElement !== document.body) {
    el = document.activeElement;
  } else throw new Error('give ref (from snapshot), selector or text');
  window.__ncur = el;
  if (spec.scroll !== false) el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  const x = r.left + r.width / 2, y = r.top + r.height / 2;
  let covered = '';
  const top = document.elementFromPoint(x, y);
  if (top && top !== el && !el.contains(top) && !top.contains(el)) {
    covered = top.tagName.toLowerCase() + (top.id ? '#' + top.id : '') +
      (typeof top.className === 'string' && top.className.trim() ? '.' + top.className.trim().split(/\s+/).slice(0, 2).join('.') : '');
  }
  const tag = el.tagName.toLowerCase();
  const n = nameOf(el);
  return { x, y, l: r.left, t: r.top, w: r.width, h: r.height, covered, tag,
           desc: `${tag}${el.id ? '#' + el.id : ''}${n ? ' "' + n.slice(0, 60) + '"' : ''}`,
           sx: scrollX, sy: scrollY };
})(__SPEC__)"#;

/// Focus window.__ncur and place the caret: select all (clear) or at the end.
pub const FOCUS: &str = r#"((clear) => {
  const el = window.__ncur; if (!el) throw new Error('no element');
  el.focus({ preventScroll: true });
  const t = el.tagName;
  if (t === 'INPUT' || t === 'TEXTAREA') {
    if (clear) { try { el.select(); } catch (e) {
      const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set; set.call(el, '');
      el.dispatchEvent(new Event('input', { bubbles: true })); } }
    else { try { const n = el.value.length; el.setSelectionRange(n, n); } catch (e) {} }
  } else if (el.isContentEditable) {
    const r = document.createRange(); r.selectNodeContents(el); if (!clear) r.collapse(false);
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
  }
  const a = document.activeElement;
  return a === el || el.contains(a) || (a && a.shadowRoot && a === el.getRootNode().host);
})(__CLEAR__)"#;

pub const SELECT: &str = r#"((v) => {
  const el = window.__ncur;
  if (!el || el.tagName !== 'SELECT') throw new Error('that element is not a <select>; click it and pick the option instead');
  const want = String(v).trim().toLowerCase(), opts = [...el.options];
  const o = opts.find(o => o.value.toLowerCase() === want) || opts.find(o => o.text.trim().toLowerCase() === want) || opts.find(o => o.text.toLowerCase().includes(want));
  if (!o) throw new Error(`no option "${v}"; options: ${opts.map(o => o.text.trim()).join(' | ')}`);
  el.value = o.value; o.selected = true;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return o.text.trim();
})(__VALUE__)"#;

pub const WAIT: &str = r#"((q) => {
  let ok;
  if (q.selector) { const e = document.querySelector(q.selector); ok = !!e && (e.checkVisibility ? e.checkVisibility() : true); }
  else if (q.text) ok = ((document.body && document.body.innerText) || '').toLowerCase().includes(q.text.toLowerCase());
  else if (q.url) ok = location.href.includes(q.url);
  else ok = document.readyState === 'complete';
  return q.gone ? !ok : ok;
})(__Q__)"#;

pub const TEXT: &str = r#"((sel, off, max) => {
  const el = sel ? document.querySelector(sel) : (document.body || document.documentElement);
  if (!el) throw new Error('nothing matches selector ' + sel);
  const t = (el.innerText || el.textContent || '').replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n');
  return { total: t.length, text: t.slice(off, off + max), title: document.title, url: location.href };
})(__SEL__, __OFF__, __MAX__)"#;

pub const HTML: &str = r#"((sel) => {
  const el = sel ? document.querySelector(sel) : document.documentElement;
  if (!el) throw new Error('nothing matches selector ' + sel);
  return el.outerHTML;
})(__SEL__)"#;

pub const STATE: &str = "[document.readyState, location.href, document.title]";

fn lit<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn with_common(s: &str) -> String {
    s.replace("__COMMON__", COMMON).replace("__INTER__", &lit(&INTER))
}

pub fn snapshot(max: usize, filter: &str, only: bool) -> String {
    with_common(SNAPSHOT).replace("__MAX__", &max.to_string()).replace("__FILTER__", &lit(&filter.to_lowercase())).replace("__ONLY__", if only { "true" } else { "false" })
}

pub fn find(spec: &serde_json::Value) -> String {
    with_common(FIND).replace("__SPEC__", &spec.to_string())
}

pub fn fill(tpl: &str, key: &str, v: &serde_json::Value) -> String {
    tpl.replace(key, &v.to_string())
}
