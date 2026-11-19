// Meteor Browser content script. tab.rs wraps it per navigation as
// `(function (CFG) { <this file> })({...})` and injects it before page scripts.
//
// CFG: { hide: [css selectors], proc: [procedural filter JSON], generic: bool,
//        sb: bool (SponsorBlock), tr: "" | target language }
//
// Messages go to the browser through chrome.webview (top frame only); answers
// come back as { mb: <kind>, ... } objects. No innerHTML anywhere: YouTube
// enforces Trusted Types.
'use strict';

const isTop = window === window.top;
const wv = isTop && window.chrome && window.chrome.webview;
const post = m => { try { if (wv) wv.postMessage(m); } catch (e) {} };
const handlers = Object.create(null);
if (wv) {
  wv.addEventListener('message', e => {
    const d = e.data;
    if (d && typeof d === 'object' && typeof d.mb === 'string' && handlers[d.mb]) {
      try { handlers[d.mb](d); } catch (err) {}
    }
  });
}
const onReady = f => {
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', f, { once: true });
  else f();
};

// ------------------------------------------------------------------ cosmetic filtering

const sheet = new CSSStyleSheet();
const adopt = () => {
  try {
    const a = document.adoptedStyleSheets;
    if (!a.includes(sheet)) document.adoptedStyleSheets = [...a, sheet];
  } catch (e) {}
};
const important = decl => decl.split(';').map(s => s.trim()).filter(Boolean)
  .map(s => /!\s*important$/i.test(s) ? s : s + ' !important').join(';');

// Every rule text already in the sheet (rules can arrive again: generic
// answers, or a fresh set after the filter lists were updated).
const inSheet = new Set();
// Splits filters into plain CSS rules and procedural ones (run by JS).
const sortFilters = (hide, proc, rules, procedural) => {
  for (const s of hide || []) rules.push(s + '{display:none!important}');
  for (const raw of proc || []) {
    let f;
    try { f = JSON.parse(raw); } catch (e) { continue; }
    const ops = f.selector || [], act = f.action;
    if (ops.length === 1 && ops[0].type === 'css-selector' && (!act || act.type === 'style')) {
      rules.push(ops[0].arg + '{' + (act ? important(act.arg) : 'display:none!important') + '}');
    } else if (ops.length && !inSheet.has(raw)) {
      inSheet.add(raw);
      procedural.push(f);
    }
  }
};
const procedural = [];
{
  const rules = [];
  sortFilters(CFG.hide, CFG.proc, rules, procedural);
  for (const r of rules) inSheet.add(r);
  // One rule per selector: an invalid selector only drops itself.
  try { sheet.replaceSync(rules.join('\n')); } catch (e) {}
  adopt();
}

const addRules = rules => {
  for (const r of rules) {
    if (inSheet.has(r)) continue;
    inSheet.add(r);
    try { sheet.insertRule(r, sheet.cssRules.length); } catch (e) {}
  }
  adopt();
};
const addHide = sels => addRules(sels.map(s => s + '{display:none!important}'));

// Procedural operators (uBO syntax, as parsed by adblock-rust).
const reOf = s => {
  const m = /^\/(.+)\/([dgimsuy]*)$/s.exec(s);
  if (m) { try { return new RegExp(m[1], m[2]); } catch (e) {} }
  return null;
};
const unquote = s => (s = s.trim(), /^(["']).*\1$/s.test(s) ? s.slice(1, -1) : s);
const textTest = s => { const re = reOf(s); return re ? t => re.test(t) : t => t.includes(s); };
const exactTest = s => { s = unquote(s); const re = reOf(s); return re ? t => re.test(t) : t => t === s; };
const uniq = a => [...new Set(a)];

function compileOp(op) {
  const arg = op.arg;
  switch (op.type) {
    case 'css-selector': {
      const sel = arg.trim();
      const comb = /^[>+~]/.test(sel) ? sel[0] : /^\s/.test(arg) ? ' ' : '';
      const rest = comb && comb !== ' ' ? sel.slice(1).trim() : sel;
      return nodes => {
        if (nodes === null) return [...document.querySelectorAll(sel)];
        const out = [];
        for (const n of nodes) {
          if (comb === '+') {
            const s = n.nextElementSibling;
            if (s && s.matches(rest)) out.push(s);
          } else if (comb === '~') {
            for (let s = n.nextElementSibling; s; s = s.nextElementSibling) if (s.matches(rest)) out.push(s);
          } else if (comb) {
            out.push(...n.querySelectorAll(':scope ' + sel));
          } else if (n.matches(sel)) {
            out.push(n);
          }
        }
        return uniq(out);
      };
    }
    case 'has-text': {
      const t = textTest(arg);
      return nodes => (nodes || []).filter(n => t(n.textContent));
    }
    case 'min-text-length': {
      const min = parseInt(arg, 10) || 0;
      return nodes => (nodes || []).filter(n => n.textContent.length >= min);
    }
    case 'matches-path': {
      const t = textTest(arg);
      return nodes => t(location.pathname + location.search) ? nodes : [];
    }
    case 'matches-attr': {
      const m = /^\s*(\/.+?\/[a-z]*|"[^"]*"|'[^']*'|[^=\s]+)\s*(?:=\s*([\s\S]+?))?\s*$/.exec(arg);
      if (!m) return () => [];
      const name = exactTest(m[1]), value = m[2] === undefined ? () => true : exactTest(m[2]);
      return nodes => (nodes || []).filter(n => {
        for (const a of n.attributes) if (name(a.name) && value(a.value)) return true;
        return false;
      });
    }
    case 'matches-css':
    case 'matches-css-before':
    case 'matches-css-after': {
      const i = arg.indexOf(':');
      if (i < 0) return () => [];
      const prop = arg.slice(0, i).trim(), value = exactTest(arg.slice(i + 1));
      const pseudo = op.type === 'matches-css' ? null : op.type.endsWith('before') ? '::before' : '::after';
      return nodes => (nodes || []).filter(n => value(getComputedStyle(n, pseudo).getPropertyValue(prop)));
    }
    case 'upward': {
      const k = /^\d+$/.test(arg.trim()) ? parseInt(arg, 10) : 0;
      return nodes => uniq((nodes || []).map(n => {
        if (k) { let e = n; for (let i = 0; i < k && e; i++) e = e.parentElement; return e; }
        return n.parentElement && n.parentElement.closest(arg);
      }).filter(Boolean));
    }
    case 'xpath':
      return nodes => {
        const out = [];
        for (const ctx of nodes || [document]) {
          try {
            const r = document.evaluate(arg, ctx, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
            for (let i = 0; i < r.snapshotLength; i++) if (r.snapshotItem(i).nodeType === 1) out.push(r.snapshotItem(i));
          } catch (e) { return []; }
        }
        return uniq(out);
      };
  }
  return () => [];
}

const compileProc = f => {
  const ops = f.selector.map(compileOp);
  return { act: f.action, run: () => { let nodes = null; for (const o of ops) { nodes = o(nodes); if (nodes && !nodes.length) return []; } return nodes || []; } };
};
const procs = procedural.map(compileProc);

function applyProcedural() {
  for (const p of procs) {
    let found;
    try { found = p.run(); } catch (e) { continue; }
    const a = p.act;
    for (const el of found) {
      if (!a) {
        if (el.style.getPropertyValue('display') !== 'none') el.style.setProperty('display', 'none', 'important');
      } else if (a.type === 'remove') {
        el.remove();
      } else if (a.type === 'style') {
        for (const d of a.arg.split(';')) {
          const i = d.indexOf(':');
          if (i > 0) el.style.setProperty(d.slice(0, i).trim(), d.slice(i + 1).replace(/!\s*important/i, '').trim(), 'important');
        }
      } else if (a.type === 'remove-attr') {
        for (const n of a.arg.split('|')) if (el.hasAttribute(n.trim())) el.removeAttribute(n.trim());
      } else if (a.type === 'remove-class') {
        for (const n of a.arg.split('|')) if (el.classList.contains(n.trim())) el.classList.remove(n.trim());
      }
    }
  }
}

// Generic rules (##.ad-box, ###banner): ask the browser about every class and
// id that appears on the page.
const seen = new Set();
let qc = [], qi = [], flushTimer = 0, procTimer = 0;
const generic = !!(CFG.generic && wv);
const addNames = el => {
  const id = el.id;
  if (id && typeof id === 'string' && !seen.has('#' + id)) { seen.add('#' + id); qi.push(id); }
  const cl = el.classList;
  if (cl) for (const c of cl) if (!seen.has('.' + c)) { seen.add('.' + c); qc.push(c); }
};
const scan = root => {
  if (root.nodeType !== 1) return;
  addNames(root);
  for (const el of root.querySelectorAll('[id],[class]')) addNames(el);
};
const flush = () => {
  flushTimer = 0;
  if (!qc.length && !qi.length) return;
  post({ mb: 'cos', c: qc, i: qi });
  qc = []; qi = [];
};
if (generic) handlers.cos = d => { if (Array.isArray(d.s) && d.s.length) addHide(d.s); };

let mo = null;
const observe = () => {
  if (mo) return;
  mo = new MutationObserver(records => {
    if (generic) {
      for (const r of records) {
        if (r.type === 'attributes') addNames(r.target);
        else for (const n of r.addedNodes) scan(n);
      }
      if ((qc.length || qi.length) && !flushTimer) flushTimer = setTimeout(flush, 40);
    }
    if (procs.length && !procTimer) procTimer = setTimeout(() => { procTimer = 0; applyProcedural(); }, 120);
  });
  mo.observe(document, { childList: true, subtree: true, attributes: generic, attributeFilter: generic ? ['class', 'id'] : undefined });
};
if (generic || procs.length) {
  observe();
  onReady(() => {
    adopt();
    if (generic) { scan(document.documentElement); flush(); }
    if (procs.length) applyProcedural();
  });
  window.addEventListener('load', () => { adopt(); if (procs.length) applyProcedural(); }, { once: true });
}

// Filter lists were updated while this page was open (e.g. the very first
// download after installing): the browser sends the site's new rules.
if (wv) handlers.hide = d => {
  const rules = [], fresh = [];
  sortFilters(d.hide, d.proc, rules, fresh);
  addRules(rules);
  if (fresh.length) {
    procs.push(...fresh.map(compileProc));
    observe();
    applyProcedural();
  }
};

// ------------------------------------------------------------------ SponsorBlock

if (CFG.sb && wv && /(^|\.)youtube\.com$/.test(location.hostname)) sponsorBlock();

function sponsorBlock() {
  const COLORS = {
    sponsor: '#00d400', selfpromo: '#ffff00', interaction: '#cc00ff', intro: '#00ffff', outro: '#0202ed',
    preview: '#008fd6', music_offtopic: '#ff9900', filler: '#7300ff',
  };
  const NAMES = {
    sponsor: 'реклама спонсора', selfpromo: 'самореклама', interaction: 'просьба подписаться', intro: 'заставка',
    outro: 'концовка', preview: 'анонс', music_offtopic: 'фрагмент без музыки', filler: 'отступление',
  };
  let vid = '', segs = [], video = null, bar = null, mutedByUs = false, timer = 0;
  const waiting = new Map();
  handlers.sb = d => {
    const cbs = waiting.get(d.p) || [];
    waiting.delete(d.p);
    let data = [];
    try { data = JSON.parse(d.d || '[]'); } catch (e) {}
    for (const f of cbs) f(data);
  };
  const sha256 = async s => {
    const b = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(s));
    return [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, '0')).join('');
  };
  const videoId = () => location.pathname === '/watch' ? new URLSearchParams(location.search).get('v') || '' : '';

  async function load(id) {
    // Only a 4-character hash prefix leaves the browser (SponsorBlock's private API).
    const p = (await sha256(id)).slice(0, 4);
    const data = await new Promise(res => {
      if (!waiting.has(p)) { waiting.set(p, []); post({ mb: 'sb', p }); }
      waiting.get(p).push(res);
    });
    if (id !== vid) return;
    const e = Array.isArray(data) ? data.find(x => x.videoID === id) : null;
    segs = e && Array.isArray(e.segments) ? e.segments
      .filter(s => (s.actionType === 'skip' || s.actionType === 'mute') && Array.isArray(s.segment) && s.segment[1] - s.segment[0] >= 1)
      .map(s => ({ a: +s.segment[0], b: +s.segment[1], cat: s.category, mute: s.actionType === 'mute', undo: false })) : [];
    drawBar();
    check();
  }

  function check() {
    clearTimeout(timer);
    if (!video || !segs.length || video.paused && !video.seeking) return;
    if (video.closest('.ad-showing')) return;
    const t = video.currentTime;
    let inMute = false, next = Infinity;
    for (const s of segs) {
      if (t >= s.a && t < s.b - 0.3) {
        if (s.mute) { inMute = true; continue; }
        if (s.undo) continue;
        video.currentTime = s.b;
        toast(s);
        return;
      }
      if (s.a > t) next = Math.min(next, s.a);
    }
    if (inMute && !video.muted) { video.muted = true; mutedByUs = true; }
    if (!inMute && mutedByUs) { video.muted = false; mutedByUs = false; }
    // timeupdate fires ~4 times a second; aim precisely at the next start.
    const ms = (next - t) / (video.playbackRate || 1) * 1000;
    if (ms < 1500) timer = setTimeout(check, Math.max(0, ms));
  }

  function drawBar() {
    if (bar) bar.remove();
    bar = null;
    const host = document.querySelector('.ytp-progress-bar');
    const dur = video && video.duration;
    if (!host || !segs.length || !(dur > 0)) return;
    bar = document.createElement('div');
    bar.style.cssText = 'position:absolute;left:0;right:0;top:0;bottom:0;pointer-events:none;z-index:40';
    for (const s of segs) {
      const d = document.createElement('div');
      d.style.cssText = `position:absolute;top:0;bottom:0;left:${s.a / dur * 100}%;width:${(s.b - s.a) / dur * 100}%;` +
        `background:${COLORS[s.cat] || '#ccc'};opacity:.85`;
      bar.append(d);
    }
    host.append(bar);
  }

  let tip = null, tipText = null, tipBtn = null, tipTimer = 0, tipSeg = null;
  function toast(s) {
    if (!tip) {
      tip = document.createElement('div');
      tip.style.cssText = 'position:absolute;left:12px;bottom:72px;z-index:2147483000';
      const root = tip.attachShadow({ mode: 'closed' });
      const st = document.createElement('style');
      st.textContent = `.b{display:flex;gap:10px;align-items:center;background:rgba(20,20,24,.92);color:#fff;font:500 13px/1.2 system-ui,sans-serif;
        padding:8px 10px 8px 12px;border-radius:10px;box-shadow:0 4px 16px rgba(0,0,0,.4)}
        button{all:unset;cursor:pointer;color:#7cc4ff;font-weight:600;padding:2px 4px;border-radius:6px}
        button:hover{background:rgba(255,255,255,.1)}`;
      const box = document.createElement('div');
      box.className = 'b';
      tipText = document.createElement('span');
      tipBtn = document.createElement('button');
      tipBtn.textContent = 'Отменить';
      tipBtn.addEventListener('click', e => {
        e.stopPropagation();
        if (tipSeg && video) { tipSeg.undo = true; video.currentTime = tipSeg.a; }
        tip.remove();
      });
      box.append(tipText, tipBtn);
      root.append(st, box);
    }
    tipSeg = s;
    tipText.textContent = 'Пропущено: ' + (NAMES[s.cat] || s.cat);
    const host = document.querySelector('#movie_player') || document.body;
    if (tip.parentNode !== host) host.append(tip);
    clearTimeout(tipTimer);
    tipTimer = setTimeout(() => tip.remove(), 5000);
  }

  const bind = v => {
    if (v === video) return;
    if (video) for (const ev of ['timeupdate', 'seeked', 'play']) video.removeEventListener(ev, check);
    if (video) video.removeEventListener('durationchange', drawBar);
    video = v;
    if (video) {
      for (const ev of ['timeupdate', 'seeked', 'play']) video.addEventListener(ev, check);
      video.addEventListener('durationchange', drawBar);
    }
  };
  setInterval(() => {
    bind(document.querySelector('#movie_player video, video.html5-main-video') || document.querySelector('video'));
    const id = videoId();
    if (id !== vid) {
      vid = id; segs = []; drawBar();
      if (mutedByUs && video) { video.muted = false; mutedByUs = false; }
      if (id) load(id);
    } else if (segs.length && bar && !bar.isConnected) {
      drawBar(); // YouTube re-rendered the player
    }
  }, 1000);
}

// ------------------------------------------------------------------ page translation

if (wv) translator();

function translator() {
  const SKIP = 'script,style,noscript,textarea,code,pre,kbd,samp,var,svg,math,select,option,template,' +
    '[translate="no"],.notranslate,[contenteditable=""],[contenteditable="true"]';
  const INLINE = new Set(['A', 'ABBR', 'B', 'BDI', 'BDO', 'BIG', 'CITE', 'DATA', 'DEL', 'DFN', 'EM', 'FONT', 'I', 'INS',
    'LABEL', 'MARK', 'Q', 'S', 'SMALL', 'SPAN', 'STRIKE', 'STRONG', 'SUB', 'SUP', 'TIME', 'TT', 'U']);
  let on = false, to = 'ru', seq = 0, io = null, mo = null;
  const orig = new Map(), done = new Map(), blocks = new Map(), inflight = new Map();
  let queue = [];

  const esc = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const ENT = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };
  const decode = s => s.replace(/<[^>]*>/g, '').replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, e) => {
    const l = e.toLowerCase();
    if (l[0] === '#') {
      const c = l[1] === 'x' ? parseInt(l.slice(2), 16) : parseInt(l.slice(1), 10);
      try { return String.fromCodePoint(c); } catch (err) { return m; }
    }
    return ENT[l] || m;
  });

  const blockOf = n => {
    let e = n.parentElement;
    while (e && INLINE.has(e.tagName) && e.parentElement && e.parentElement !== document.documentElement) e = e.parentElement;
    return e;
  };
  const want = n => {
    if (orig.has(n) || !/\p{L}/u.test(n.nodeValue)) return false;
    const p = n.parentElement;
    return !!p && !p.closest(SKIP);
  };
  function collect(root) {
    const found = [];
    if (root.nodeType === 3) { if (want(root)) found.push(root); }
    else if (root.nodeType === 1 && !root.closest(SKIP)) {
      const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      for (let n = w.nextNode(); n; n = w.nextNode()) if (want(n)) found.push(n);
    }
    for (const n of found) {
      const b = blockOf(n);
      if (!b) continue;
      const list = blocks.get(b);
      if (list) { if (!list.includes(n)) list.push(n); }
      else { blocks.set(b, [n]); io.observe(b); }
    }
  }

  function enqueue(nodes) {
    let unit = [], len = 0;
    const push = () => { if (unit.length) queue.push(unit); unit = []; len = 0; };
    for (const n of nodes) {
      if (!n.isConnected || orig.has(n)) continue;
      const v = n.nodeValue;
      if (len + v.length > 1800) push();
      unit.push(n); len += v.length;
    }
    push();
  }
  const unitHtml = u => u.length === 1 ? esc(u[0].nodeValue.trim())
    : u.map((n, i) => `<a i=${i}>${esc(n.nodeValue.trim())}</a>`).join('');

  function pump() {
    while (on && inflight.size < 3 && queue.length) {
      const batch = [];
      let len = 0;
      while (queue.length && batch.length < 40 && len < 4000) { const u = queue.shift(); batch.push(u); len += unitHtml(u).length; }
      const id = ++seq;
      inflight.set(id, batch);
      post({ mb: 'tr', id, to, q: batch.map(unitHtml) });
    }
  }

  function set(n, text) {
    if (!n.isConnected) return;
    const v = n.nodeValue;
    const lead = /^\s*/.exec(v)[0], trail = /\s*$/.exec(v)[0];
    if (!orig.has(n)) orig.set(n, v);
    const t = lead + text.trim() + trail;
    done.set(n, t);
    n.nodeValue = t;
  }
  function apply(unit, html) {
    if (typeof html !== 'string') return;
    if (unit.length === 1) { set(unit[0], decode(html)); return; }
    const re = /<a\s+i\s*=\s*"?(\d+)"?\s*>([\s\S]*?)<\/a\s*>/gi;
    let m;
    while ((m = re.exec(html))) { const n = unit[+m[1]]; if (n) set(n, decode(m[2])); }
  }

  handlers.tr = d => {
    if (d.on === true) start(d.to || to);
    else if (d.on === false) stop();
    else if (d.id && inflight.has(d.id)) {
      const batch = inflight.get(d.id);
      inflight.delete(d.id);
      if (on && Array.isArray(d.r) && d.r.length === batch.length) batch.forEach((u, i) => apply(u, d.r[i]));
      pump();
    }
  };

  function start(lang) {
    if (on) return;
    on = true; to = lang;
    io = new IntersectionObserver(entries => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        const nodes = blocks.get(e.target);
        blocks.delete(e.target);
        io.unobserve(e.target);
        if (nodes) enqueue(nodes);
      }
      pump();
    }, { rootMargin: '800px 0px' });
    mo = new MutationObserver(records => {
      for (const r of records) {
        if (r.type === 'characterData') {
          const n = r.target;
          if (done.has(n) && done.get(n) !== n.nodeValue) { orig.delete(n); done.delete(n); collect(n); }
        } else {
          for (const n of r.addedNodes) collect(n);
        }
      }
    });
    onReady(() => {
      if (!on) return;
      collect(document.body || document.documentElement);
      mo.observe(document.body || document.documentElement, { childList: true, subtree: true, characterData: true });
    });
  }

  function stop() {
    on = false;
    if (io) io.disconnect();
    if (mo) mo.disconnect();
    io = mo = null;
    for (const [n, v] of orig) if (n.isConnected && done.get(n) === n.nodeValue) n.nodeValue = v;
    orig.clear(); done.clear(); blocks.clear(); inflight.clear(); queue = [];
  }

  if (CFG.tr) start(CFG.tr);

  // Tell the browser whether the page looks foreign (shows the translate button).
  const report = () => {
    const root = document.body;
    if (!root) return;
    const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let letters = 0, cyr = 0;
    for (let n = w.nextNode(); n && letters < 4000; n = w.nextNode()) {
      const p = n.parentElement;
      if (!p || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(p.tagName)) continue;
      const v = n.nodeValue;
      letters += (v.match(/\p{L}/gu) || []).length;
      cyr += (v.match(/[Ѐ-ӿ]/g) || []).length;
    }
    post({ mb: 'lang', l: (document.documentElement.lang || '').toLowerCase(), letters, cyr });
  };
  if (document.readyState === 'complete') setTimeout(report, 300);
  else window.addEventListener('load', () => setTimeout(report, 300), { once: true });
}
