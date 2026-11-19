//! Management pages: bookmarks (mb:bookmarks) and settings (mb:settings), plus the
//! validation of what they are allowed to change.
//!
//! The pages talk to the app with `chrome.webview.postMessage({mb:"int", t:<token>, a:<action>, ..})`.
//! `App::internal_message` only accepts such a message from a tab that shows one of these pages,
//! with this session's random token (it exists only inside the page's own HTML) and from a
//! non-http document, so a website cannot change settings or bookmarks. Everything that
//! arrives is validated again here: the page is not trusted either.

use std::hash::{BuildHasher, Hasher};

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    pages::Tile,
    store::{self, Bookmark, Bookmarks, Settings},
};

pub const BOOKMARKS_URL: &str = "mb:bookmarks";
pub const SETTINGS_URL: &str = "mb:settings";

/// JSON safe to embed inside <script>.
fn embed<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into()).replace("</", "<\\/")
}

/// 128 random bits as hex: std's hasher keys come from the OS random generator.
pub fn new_token() -> String {
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(store::now());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

// ---------------------------------------------------------------- bookmarks

#[derive(Serialize)]
struct BmRow<'a> {
    u: &'a str,
    t: &'a str,
    h: &'a str,
    i: &'a str,
    a: u64,
}

/// `tiles` are the same bookmarks with their (cached) icons filled in.
pub fn bookmarks_page(items: &[Bookmark], tiles: &[Tile], token: &str) -> String {
    let rows: Vec<BmRow> = items
        .iter()
        .zip(tiles)
        .map(|(b, t)| BmRow { u: &b.url, t: &b.title, h: &t.host, i: &t.icon, a: b.added })
        .collect();
    BOOKMARKS.replace("/*DATA*/null", &embed(&rows)).replace("/*TOKEN*/\"\"", &embed(&token))
}

fn http_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://")) && u.len() <= 4000 && !u.contains(char::is_whitespace)
}

/// Applies one bookmark action from the page. Returns true when something changed.
pub fn bookmark_op(b: &mut Bookmarks, a: &str, v: &Value) -> bool {
    let url = v["url"].as_str().unwrap_or("");
    match a {
        "bm_rename" => {
            let title: String = v["title"].as_str().unwrap_or("").trim().chars().take(300).collect();
            match b.items.iter_mut().find(|x| x.url == url) {
                Some(x) if !title.is_empty() && x.title != title => {
                    x.title = title;
                    true
                }
                _ => false,
            }
        }
        "bm_delete" => match b.items.iter().position(|x| x.url == url) {
            Some(i) => {
                b.items.remove(i);
                true
            }
            None => false,
        },
        "bm_restore" => {
            if !http_url(url) || b.contains(url) {
                return false;
            }
            let title: String = v["title"].as_str().unwrap_or("").chars().take(300).collect();
            let at = (v["at"].as_u64().unwrap_or(u64::MAX) as usize).min(b.items.len());
            let added = v["added"].as_u64().unwrap_or_else(store::now);
            b.items.insert(at, Bookmark { url: url.into(), title, added });
            true
        }
        "bm_order" => {
            let Some(urls) = v["urls"].as_array() else { return false };
            let mut old = std::mem::take(&mut b.items);
            let mut new = Vec::with_capacity(old.len());
            for u in urls.iter().filter_map(Value::as_str) {
                if let Some(i) = old.iter().position(|x| x.url == u) {
                    new.push(old.remove(i));
                }
            }
            new.append(&mut old); // whatever the page did not mention keeps its place at the end
            b.items = new;
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- settings

pub const SB_CATEGORIES: [&str; 8] =
    ["sponsor", "selfpromo", "interaction", "intro", "outro", "preview", "music_offtopic", "filler"];

/// What the app has to do besides saving the file.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Effect {
    None,
    /// Master ad-block switch: engine flag + reload the page.
    AdblockSwitch,
    /// Lists / cookie banners / annoyances: recompile the engine.
    AdblockRules,
    SponsorSwitch,
    /// Cached SponsorBlock answers were made for the old category list.
    SponsorCategories,
    Tracking,
    Tablo,
}

/// Validates and applies one setting. `None` = rejected (settings untouched).
pub fn set_setting(s: &mut Settings, key: &str, v: &Value) -> Option<Effect> {
    let text = v.as_str();
    let flag = v.as_bool();
    let one_of = |opts: &[&str]| text.filter(|t| opts.contains(t)).map(String::from);
    match key {
        "search" => {
            let t = text?.trim();
            let ok = matches!(t, "google" | "yandex" | "duckduckgo" | "bing")
                || (t.contains("{q}") && t.len() <= 300 && http_url(t));
            if !ok {
                return None;
            }
            s.search = t.into();
        }
        "adblock" => {
            s.adblock = flag?;
            return Some(Effect::AdblockSwitch);
        }
        "adblock_cookies" => {
            s.adblock_cookies = flag?;
            return Some(Effect::AdblockRules);
        }
        "adblock_annoyances" => {
            s.adblock_annoyances = flag?;
            return Some(Effect::AdblockRules);
        }
        "adblock_lists" => {
            let mut lists: Vec<String> = Vec::new();
            for u in v.as_array()?.iter().filter_map(Value::as_str).map(str::trim) {
                if http_url(u) && u.len() <= 500 && !lists.iter().any(|x| x == u) && lists.len() < 50 {
                    lists.push(u.into());
                }
            }
            s.adblock_lists = lists;
            return Some(Effect::AdblockRules);
        }
        "tracking_prevention" => {
            s.tracking_prevention = one_of(&["none", "basic", "balanced", "strict"])?;
            return Some(Effect::Tracking);
        }
        "sleep_minutes" => {
            let n = v.as_u64().filter(|n| *n <= 1440)?;
            s.sleep_minutes = n;
        }
        "restore_session" => s.restore_session = flag?,
        "translate_to" => {
            let t = text?;
            if !crate::translate::valid_lang(t) {
                return None;
            }
            s.translate_to = t.into();
        }
        "sponsorblock" => {
            s.sponsorblock = flag?;
            return Some(Effect::SponsorSwitch);
        }
        "sponsorblock_categories" => {
            let want: Vec<&str> = v.as_array()?.iter().filter_map(Value::as_str).collect();
            // Keep the canonical order, drop unknown names and duplicates.
            s.sponsorblock_categories =
                SB_CATEGORIES.iter().filter(|c| want.contains(c)).map(|c| c.to_string()).collect();
            return Some(Effect::SponsorCategories);
        }
        "tablo_size" => {
            s.tablo_size = one_of(&["small", "medium", "large"])?;
            return Some(Effect::Tablo);
        }
        "tablo_width" => {
            s.tablo_width = one_of(&["auto", "small", "medium", "large"])?;
            return Some(Effect::Tablo);
        }
        _ => return None,
    }
    Some(Effect::None)
}

/// The settings the page shows (live_blur and the rest stay hidden).
pub fn settings_json(s: &Settings) -> Value {
    json!({
        "search": s.search, "adblock": s.adblock, "adblock_cookies": s.adblock_cookies,
        "adblock_annoyances": s.adblock_annoyances, "adblock_lists": s.adblock_lists,
        "tracking_prevention": s.tracking_prevention, "sleep_minutes": s.sleep_minutes,
        "restore_session": s.restore_session, "translate_to": s.translate_to,
        "sponsorblock": s.sponsorblock, "sponsorblock_categories": s.sponsorblock_categories,
        "tablo_size": s.tablo_size, "tablo_width": s.tablo_width,
    })
}

pub fn settings_page(s: &Settings, token: &str) -> String {
    let about = json!({
        "name": crate::APP_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "webview": wry::webview_version().unwrap_or_default(),
        "dir": store::data_dir().display().to_string(),
    });
    SETTINGS
        .replace("/*DATA*/null", &embed(&json!({ "s": settings_json(s), "about": about })))
        .replace("/*TOKEN*/\"\"", &embed(&token))
}

const BOOKMARKS: &str = r##"<!doctype html><html lang="ru"><head><meta charset="utf-8"><title>Закладки</title>
<style>
:root{--bg:#111216;--fg:#e9e9ee;--mut:#8b8d98;--card:#1b1c22;--acc:#58b4f0;color-scheme:dark}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif}
main{max-width:860px;margin:0 auto;padding:32px 16px 80px}
h1{font:600 26px "Segoe UI Variable Display","Segoe UI",system-ui;margin:0 0 18px;display:flex;align-items:baseline;gap:12px}
h1 small{font:400 13px "Segoe UI",system-ui;color:var(--mut)}
#q{width:100%;height:44px;border-radius:22px;border:1px solid #ffffff14;background:var(--card);color:var(--fg);font:inherit;font-size:15px;padding:0 20px;outline:none;margin-bottom:14px}
#q:focus{border-color:var(--acc)}
.row{display:flex;align-items:center;border-radius:10px;position:relative}
.row:hover{background:var(--card)}
.row.drag{opacity:.35}
.row.over-top::before,.row.over-bottom::after{content:"";position:absolute;left:8px;right:8px;height:2px;background:var(--acc);border-radius:1px}
.row.over-top::before{top:-1px}.row.over-bottom::after{bottom:-1px}
a.main{flex:1;min-width:0;display:flex;gap:14px;align-items:center;padding:8px 12px;color:var(--fg);text-decoration:none}
.ic{width:22px;height:22px;flex:none;display:grid;place-items:center;border-radius:6px;background:#2a2c35;color:#aab;font:600 12px "Segoe UI",sans-serif;overflow:hidden}
.ic img{width:22px;height:22px;object-fit:contain}
.tt{flex:1;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hs{color:var(--mut);font-size:12px;max-width:34%;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.act{display:flex;gap:2px;padding-right:8px;opacity:0}
.row:hover .act,.row:focus-within .act{opacity:1}
.act button{width:30px;height:30px;border:0;border-radius:8px;background:none;color:var(--mut);cursor:pointer;display:grid;place-items:center;padding:0}
.act button:hover{background:#ffffff14;color:var(--fg)}
.act button.del:hover{color:#ff7b7b}
.act svg{width:16px;height:16px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}
input.rn{flex:1;margin:4px 8px;height:34px;border-radius:8px;border:1px solid var(--acc);background:#0c0d10;color:var(--fg);font:inherit;padding:0 10px;outline:none}
#more{display:none;margin:16px auto;background:var(--card);color:var(--fg);border:0;border-radius:18px;padding:9px 18px;font:inherit;cursor:pointer}
.empty{color:var(--mut);padding:28px 12px;line-height:1.6}
#toast{position:fixed;left:50%;bottom:28px;transform:translate(-50%,20px);background:#2a2c35;border:1px solid #ffffff1a;border-radius:12px;padding:10px 16px;display:flex;gap:14px;align-items:center;opacity:0;pointer-events:none;transition:.18s;box-shadow:0 8px 24px #0008}
#toast.on{opacity:1;transform:translate(-50%,0);pointer-events:auto}
#toast button{background:none;border:0;color:var(--acc);font:inherit;font-weight:600;cursor:pointer;padding:0}
</style></head><body><main>
<h1>Закладки <small id="cnt"></small></h1>
<input id="q" autofocus placeholder="Поиск по закладкам" spellcheck="false">
<div id="list"></div><button id="more">Показать ещё</button>
</main><div id="toast"><span id="tm"></span><button id="tu">Отменить</button></div>
<script>
const ROWS=/*DATA*/null||[], TOKEN=/*TOKEN*/"";
const wv=window.chrome&&window.chrome.webview;
const send=m=>{try{wv.postMessage(Object.assign({mb:'int',t:TOKEN},m))}catch(e){}};
const list=document.getElementById('list'),more=document.getElementById('more'),qi=document.getElementById('q'),cnt=document.getElementById('cnt');
let items=ROWS.slice(),limit=300,drag=null,undo=null,tt=0;
const SVG={
  ed:'<svg viewBox="0 0 24 24"><path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/></svg>',
  del:'<svg viewBox="0 0 24 24"><path d="M5 7h14M10 11v6M14 11v6M6 7l1 12h10l1-12M9 7V4h6v3"/></svg>'
};
function host(u){try{return new URL(u).host}catch{return u}}
function icon(r){
  const box=document.createElement('span');box.className='ic';
  const l=document.createElement('span');l.textContent=(r.t||r.h||'?').trim().charAt(0).toUpperCase();box.appendChild(l);
  const h=r.h||host(r.u);if(!h)return box;
  const ip=/^[\d.:[\]]+$/.test(h),tries=[];
  if(r.i)tries.push(r.i);
  tries.push((ip||r.u.startsWith('http:')?'http':'https')+'://'+h+'/favicon.ico');
  const img=new Image();
  img.onload=()=>{if(img.naturalWidth<2)return img.onerror();l.remove();box.appendChild(img)};
  img.onerror=()=>{const n=tries.shift();if(n)img.src=n};
  img.onerror();return box;
}
function safe(u){return /^https?:\/\//i.test(u)?u:'#'}
function button(cls,title,svg,fn){
  const b=document.createElement('button');b.className=cls;b.title=title;b.innerHTML=svg;
  b.onclick=e=>{e.preventDefault();e.stopPropagation();fn()};return b;
}
function toast(msg,undoFn){
  document.getElementById('tm').textContent=msg;
  const tu=document.getElementById('tu');tu.style.display=undoFn?'':'none';undo=undoFn||null;
  const t=document.getElementById('toast');t.classList.add('on');clearTimeout(tt);tt=setTimeout(()=>{t.classList.remove('on');undo=null},6000);
}
document.getElementById('tu').onclick=()=>{if(undo){const f=undo;undo=null;f()}document.getElementById('toast').classList.remove('on')};
function remove(r){
  const at=items.indexOf(r);if(at<0)return;
  items.splice(at,1);send({a:'bm_delete',url:r.u});render();
  toast('Закладка удалена',()=>{items.splice(Math.min(at,items.length),0,r);send({a:'bm_restore',url:r.u,title:r.t,added:r.a,at});render()});
}
function rename(r,row,main){
  const inp=document.createElement('input');inp.className='rn';inp.value=r.t||r.u;inp.spellcheck=false;
  row.replaceChild(inp,main);inp.focus();inp.select();
  let done=false;
  const end=save=>{
    if(done)return;done=true;
    const v=inp.value.trim();
    if(save&&v&&v!==r.t){r.t=v;send({a:'bm_rename',url:r.u,title:v})}
    render();
  };
  inp.onkeydown=e=>{if(e.key==='Enter')end(true);else if(e.key==='Escape'){e.stopPropagation();end(false)}};
  inp.onblur=()=>end(true);
}
function mkrow(r,canDrag){
  const row=document.createElement('div');row.className='row';
  const a=document.createElement('a');a.className='main';a.href=safe(r.u);a.title=r.u;a.draggable=false;
  const tt=document.createElement('span');tt.className='tt';tt.textContent=r.t||r.u;
  const hs=document.createElement('span');hs.className='hs';hs.textContent=r.h||host(r.u);
  a.append(icon(r),tt,hs);
  const act=document.createElement('div');act.className='act';
  act.append(button('ed','Переименовать',SVG.ed,()=>rename(r,row,a)),button('del','Удалить',SVG.del,()=>remove(r)));
  row.append(a,act);
  if(canDrag){
    row.draggable=true;
    row.ondragstart=e=>{drag=r;row.classList.add('drag');e.dataTransfer.effectAllowed='move';e.dataTransfer.setData('text/plain',r.u)};
    row.ondragend=()=>{drag=null;render()};
    row.ondragover=e=>{
      if(!drag||drag===r)return;e.preventDefault();
      const top=e.clientY<row.getBoundingClientRect().top+row.offsetHeight/2;
      row.classList.toggle('over-top',top);row.classList.toggle('over-bottom',!top);
    };
    row.ondragleave=()=>row.classList.remove('over-top','over-bottom');
    row.ondrop=e=>{
      e.preventDefault();if(!drag||drag===r)return;
      const top=e.clientY<row.getBoundingClientRect().top+row.offsetHeight/2;
      items.splice(items.indexOf(drag),1);
      items.splice(items.indexOf(r)+(top?0:1),0,drag);
      send({a:'bm_order',urls:items.map(x=>x.u)});drag=null;render();
    };
  }
  return row;
}
function render(){
  const q=qi.value.toLowerCase().split(/\s+/).filter(Boolean);
  const rows=q.length?items.filter(r=>{const s=((r.t||'')+' '+r.u).toLowerCase();return q.every(w=>s.includes(w))}):items;
  cnt.textContent=items.length?items.length+(q.length?' · найдено '+rows.length:''):'';
  list.textContent='';const frag=document.createDocumentFragment();
  for(const r of rows.slice(0,limit))frag.appendChild(mkrow(r,!q.length));
  if(!rows.length){
    const e=document.createElement('div');e.className='empty';
    e.textContent=items.length?'Ничего не найдено':'Закладок пока нет. Нажмите звёздочку в адресной строке или Ctrl+D на нужной странице.';
    frag.appendChild(e);
  }
  list.appendChild(frag);more.style.display=rows.length>limit?'block':'none';
}
qi.oninput=()=>{limit=300;render()};
more.onclick=()=>{limit+=500;render()};
render();
</script></body></html>"##;

const SETTINGS: &str = r##"<!doctype html><html lang="ru"><head><meta charset="utf-8"><title>Настройки</title>
<style>
:root{--bg:#111216;--fg:#e9e9ee;--mut:#8b8d98;--card:#1b1c22;--line:#ffffff0f;--acc:#58b4f0;color-scheme:dark}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif}
main{max-width:760px;margin:0 auto;padding:32px 16px 90px}
h1{font:600 26px "Segoe UI Variable Display","Segoe UI",system-ui;margin:0 0 22px}
h2{font-size:13px;font-weight:600;color:var(--mut);margin:26px 0 8px;text-transform:uppercase;letter-spacing:.04em}
.card{background:var(--card);border-radius:14px;overflow:hidden}
.item{display:flex;align-items:center;gap:16px;padding:13px 18px;min-height:56px}
.item+.item{border-top:1px solid var(--line)}
.lb{flex:1;min-width:0}.lb b{display:block;font-weight:400}.lb small{display:block;color:var(--mut);font-size:12px;margin-top:2px;line-height:1.4}
select,input[type=text],textarea{background:#0f1014;color:var(--fg);border:1px solid #ffffff1c;border-radius:8px;font:inherit;padding:7px 10px;outline:none}
select{min-width:190px}
select:focus,input[type=text]:focus,textarea:focus{border-color:var(--acc)}
input.cu{width:100%;margin-top:8px;display:none}
.sw{position:relative;width:42px;height:24px;flex:none}
.sw input{position:absolute;inset:0;opacity:0;margin:0;cursor:pointer;z-index:1}
.sw span{position:absolute;inset:0;border-radius:12px;background:#3a3c47;transition:.15s}
.sw span::after{content:"";position:absolute;left:3px;top:3px;width:18px;height:18px;border-radius:50%;background:#fff;transition:.15s}
.sw input:checked+span{background:var(--acc)}.sw input:checked+span::after{transform:translateX(18px)}
.sw input:focus-visible+span{outline:2px solid var(--acc);outline-offset:2px}
.cats{display:grid;grid-template-columns:1fr 1fr;gap:4px 16px;padding:4px 18px 14px}
.cats label{display:flex;gap:8px;align-items:center;cursor:pointer;padding:4px 0}
.cats input{accent-color:var(--acc);width:16px;height:16px;margin:0}
.col{padding:13px 18px;display:block}
.col textarea{width:100%;height:86px;resize:vertical;margin-top:8px;font:12px Consolas,monospace}
button.b{background:#2a2c35;color:var(--fg);border:0;border-radius:8px;padding:8px 16px;font:inherit;cursor:pointer;flex:none}
button.b:hover{background:#343642}
.sub{display:none}.sub.on{display:block}
#toast{position:fixed;left:50%;bottom:26px;transform:translate(-50%,16px);background:#2a2c35;border:1px solid #ffffff1a;border-radius:12px;padding:9px 18px;opacity:0;pointer-events:none;transition:.18s;box-shadow:0 8px 24px #0008}
#toast.on{opacity:1;transform:translate(-50%,0)}#toast.err{border-color:#ff7b7b88}
.about{padding:16px 18px;line-height:1.7;color:var(--mut)}.about b{color:var(--fg);font-weight:600}
.path{font:12px Consolas,monospace;color:var(--mut);word-break:break-all}
</style></head><body><main>
<h1>Настройки</h1><div id="root"></div>
</main><div id="toast"></div>
<script>
const DATA=/*DATA*/null, TOKEN=/*TOKEN*/"";
const wv=window.chrome&&window.chrome.webview;
const send=m=>{try{wv.postMessage(Object.assign({mb:'int',t:TOKEN},m))}catch(e){}};
const S=DATA.s,ctl={};
const h=(t,p,...c)=>{const e=document.createElement(t);if(p)for(const k in p){if(k==='class')e.className=p[k];else if(k.startsWith('on'))e[k]=p[k];else e.setAttribute(k,p[k])}e.append(...c.flat().filter(x=>x!=null));return e};
let tt=0;
function toast(msg,err){const t=document.getElementById('toast');t.textContent=msg;t.className='on'+(err?' err':'');clearTimeout(tt);tt=setTimeout(()=>t.className='',1800)}
const set=(k,v)=>send({a:'set',k,v});
const label=(t,d)=>h('div',{class:'lb'},h('b',0,t),d?h('small',0,d):null);

function sw(k,t,d){
  const i=h('input',{type:'checkbox'});i.onchange=()=>set(k,i.checked);
  ctl[k]=v=>{i.checked=!!v};
  return h('div',{class:'item'},label(t,d),h('label',{class:'sw'},i,h('span')));
}
function sel(k,t,d,opts,num){
  const s=h('select');
  const fill=cur=>{
    s.textContent='';const o=opts.slice();
    if(!o.some(x=>String(x[0])===String(cur)))o.push([cur,String(cur)]);
    for(const [v,l] of o){const e=h('option',{value:v},l);s.appendChild(e)}
    s.value=String(cur);
  };
  s.onchange=()=>set(k,num?Number(s.value):s.value);
  ctl[k]=fill;
  return h('div',{class:'item'},label(t,d),s);
}
function search(){
  const names=[['google','Google'],['yandex','Яндекс'],['duckduckgo','DuckDuckGo'],['bing','Bing'],['custom','Свой адрес…']];
  const s=h('select'),cu=h('input',{type:'text',class:'cu',placeholder:'https://example.com/search?q={q}',spellcheck:'false'});
  for(const [v,l] of names)s.appendChild(h('option',{value:v},l));
  const sync=v=>{
    const known=names.slice(0,4).some(x=>x[0]===v);
    s.value=known?v:'custom';cu.style.display=known?'none':'block';
    if(!known&&document.activeElement!==cu)cu.value=v;
  };
  s.onchange=()=>{
    if(s.value==='custom'){cu.style.display='block';cu.focus()}else set('search',s.value);
  };
  const go=()=>{const v=cu.value.trim();if(!v)return;if(!v.includes('{q}')||!/^https?:\/\//i.test(v))return toast('Адрес поиска: https://… с {q} на месте запроса',true);set('search',v)};
  cu.onkeydown=e=>{if(e.key==='Enter')go()};cu.onchange=go;
  ctl.search=sync;
  const lb=label('Поисковая система','Для запросов из адресной строки');
  return h('div',{class:'item',style:'flex-wrap:wrap'},lb,s,cu);
}
function cats(){
  const names={sponsor:'Спонсорская реклама',selfpromo:'Самореклама',interaction:'Просьбы подписаться и лайкнуть',intro:'Заставка',outro:'Концовка и титры',preview:'Превью и рекап',music_offtopic:'Музыка не по теме',filler:'Болтовня и лишнее'};
  const boxes={};
  const wrap=h('div',{class:'cats'});
  for(const k in names){
    const i=h('input',{type:'checkbox'});boxes[k]=i;
    i.onchange=()=>set('sponsorblock_categories',Object.keys(boxes).filter(x=>boxes[x].checked));
    wrap.appendChild(h('label',0,i,names[k]));
  }
  ctl.sponsorblock_categories=v=>{for(const k in boxes)boxes[k].checked=(v||[]).includes(k)};
  return wrap;
}
function lists(){
  const ta=h('textarea',{spellcheck:'false',placeholder:'https://example.com/filters.txt'});
  ta.onchange=()=>set('adblock_lists',ta.value.split(/\s+/).filter(Boolean));
  ctl.adblock_lists=v=>{if(document.activeElement!==ta)ta.value=(v||[]).join('\n')};
  return h('div',{class:'item col'},label('Свои списки фильтров','Адреса списков в формате uBlock Origin / AdBlock Plus / hosts, по одному в строке. Подхватываются сразу, правила для отдельных сайтов лежат в blocklist.txt.'),ta);
}

const root=document.getElementById('root');
const sec=(t,...rows)=>{root.append(h('h2',0,t),h('div',{class:'card'},rows))};
const MIN=[[0,'Никогда'],[5,'Через 5 минут'],[10,'Через 10 минут'],[15,'Через 15 минут'],[30,'Через 30 минут'],[60,'Через час'],[180,'Через 3 часа']];
const LANGS=[['ru','Русский'],['en','English'],['uk','Українська'],['be','Беларуская'],['kk','Қазақша'],['de','Deutsch'],['fr','Français'],['es','Español'],['it','Italiano'],['pt','Português'],['pl','Polski'],['tr','Türkçe'],['zh-CN','中文 (简体)'],['ja','日本語'],['ko','한국어']];

sec('Общие',
  search(),
  sw('restore_session','Восстанавливать вкладки при запуске','Иначе браузер каждый раз стартует с чистого окна'),
  sel('sleep_minutes','Усыплять фоновые вкладки','Освобождает память; закреплённые, приватные и вкладки со звуком не засыпают',MIN,true),
  sel('translate_to','Переводить страницы на','Язык кнопки перевода в адресной строке',LANGS));
sec('Блокировка рекламы и слежки',
  sw('adblock','Блокировать рекламу и трекеры','Движок уровня uBlock Origin; действует на новые загрузки страниц'),
  sw('adblock_cookies','Скрывать окна о cookie'),
  sw('adblock_annoyances','Скрывать раздражающие элементы','Всплывающие подписки, виджеты соцсетей, «открыть в приложении»'),
  sel('tracking_prevention','Защита от слежки','Встроенная защита движка; для открытых вкладок меняется сразу',[['none','Выключена'],['basic','Базовая'],['balanced','Сбалансированная'],['strict','Строгая']]),
  lists(),
  h('div',{class:'item'},label('Фильтры','Списки обновляются сами раз в несколько дней'),h('button',{class:'b',onclick:()=>{send({a:'adblock_update'});toast('Обновление фильтров запущено')}},'Обновить сейчас')));
sec('SponsorBlock на YouTube',
  sw('sponsorblock','Пропускать вставки','Спонсорские и другие сегменты по базе sponsor.ajay.app'),
  cats());
sec('Табло',
  sel('tablo_size','Размер плиток','',[['small','Маленькие'],['medium','Средние'],['large','Большие']]),
  sel('tablo_width','Ширина Табло','',[['auto','По ширине окна'],['small','Узкое'],['medium','Среднее'],['large','Широкое']]));
sec('Данные',
  h('div',{class:'item'},label('Папка данных','Настройки, закладки, история, сессия'),h('button',{class:'b',onclick:()=>send({a:'open_data'})},'Открыть папку')),
  h('div',{class:'item'},label('Закладки','Список, поиск, переименование и порядок'),h('button',{class:'b',onclick:()=>send({a:'open_bookmarks'})},'Открыть')));
const A=DATA.about;
root.append(h('h2',0,'О браузере'),h('div',{class:'card about'},h('b',0,A.name+' '+A.version),h('br'),'Оболочка на Rust, страницы рисует системный WebView2'+(A.webview?' '+A.webview:''),h('br'),h('span',{class:'path'},A.dir)));

function apply(s){for(const k in ctl)ctl[k](s[k])}
apply(S);
if(wv)wv.addEventListener('message',e=>{
  const m=e.data;if(!m||m.mb!=='int')return;
  if(m.s)apply(m.s);
  toast(m.ok?'Сохранено':'Не удалось сохранить',!m.ok);
});
</script></body></html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(urls: &[&str]) -> Bookmarks {
        Bookmarks { items: urls.iter().map(|u| Bookmark { url: u.to_string(), title: format!("t {u}"), added: 1 }).collect() }
    }
    fn urls(b: &Bookmarks) -> Vec<&str> {
        b.items.iter().map(|x| x.url.as_str()).collect()
    }

    #[test]
    fn bookmark_actions() {
        let mut b = bm(&["http://a/", "http://b/", "http://c/"]);
        assert!(bookmark_op(&mut b, "bm_rename", &json!({"url":"http://b/","title":"  New  "})));
        assert_eq!(b.items[1].title, "New");
        assert!(!bookmark_op(&mut b, "bm_rename", &json!({"url":"http://b/","title":"   "})));
        assert!(!bookmark_op(&mut b, "bm_rename", &json!({"url":"http://zzz/","title":"x"})));
        assert!(bookmark_op(&mut b, "bm_delete", &json!({"url":"http://a/"})));
        assert_eq!(urls(&b), ["http://b/", "http://c/"]);
        assert!(bookmark_op(&mut b, "bm_restore", &json!({"url":"http://a/","title":"A","added":5,"at":0})));
        assert_eq!(urls(&b), ["http://a/", "http://b/", "http://c/"]);
        assert!(!bookmark_op(&mut b, "bm_restore", &json!({"url":"http://a/","title":"A","at":0})), "no duplicates");
        assert!(!bookmark_op(&mut b, "bm_restore", &json!({"url":"javascript:alert(1)","title":"x"})));
        assert!(bookmark_op(&mut b, "bm_order", &json!({"urls":["http://c/","http://nope/","http://a/"]})));
        assert_eq!(urls(&b), ["http://c/", "http://a/", "http://b/"]);
        assert!(!bookmark_op(&mut b, "bm_wipe", &json!({})));
    }

    #[test]
    fn settings_are_validated() {
        let mut s = Settings::default();
        assert_eq!(set_setting(&mut s, "search", &json!("yandex")), Some(Effect::None));
        assert_eq!(s.search, "yandex");
        assert_eq!(set_setting(&mut s, "search", &json!("https://x.org/?q={q}")), Some(Effect::None));
        assert_eq!(set_setting(&mut s, "search", &json!("javascript:{q}")), None);
        assert_eq!(set_setting(&mut s, "search", &json!("https://x.org/")), None);
        assert_eq!(s.search, "https://x.org/?q={q}");
        assert_eq!(set_setting(&mut s, "sleep_minutes", &json!(30)), Some(Effect::None));
        assert_eq!(s.sleep_minutes, 30);
        assert_eq!(set_setting(&mut s, "sleep_minutes", &json!(-1)), None);
        assert_eq!(set_setting(&mut s, "sleep_minutes", &json!(99999)), None);
        assert_eq!(set_setting(&mut s, "adblock_cookies", &json!(false)), Some(Effect::AdblockRules));
        assert!(!s.adblock_cookies);
        assert_eq!(set_setting(&mut s, "adblock", &json!("yes")), None);
        assert_eq!(set_setting(&mut s, "tracking_prevention", &json!("strict")), Some(Effect::Tracking));
        assert_eq!(set_setting(&mut s, "tracking_prevention", &json!("off")), None);
        assert_eq!(s.tracking_prevention, "strict");
        assert_eq!(set_setting(&mut s, "translate_to", &json!("en&x=1")), None);
        assert_eq!(set_setting(&mut s, "live_blur", &json!(true)), None, "hidden setting is not writable");
        assert!(!s.live_blur);
        assert_eq!(set_setting(&mut s, "shield_off", &json!(["a.com"])), None);
        set_setting(&mut s, "sponsorblock_categories", &json!(["outro", "bogus", "sponsor", "sponsor"]));
        assert_eq!(s.sponsorblock_categories, ["sponsor", "outro"]);
        set_setting(&mut s, "adblock_lists", &json!(["https://a.org/l.txt", "ftp://x", "https://a.org/l.txt", "file:///c:/x"]));
        assert_eq!(s.adblock_lists, ["https://a.org/l.txt"]);
        assert_eq!(set_setting(&mut s, "tablo_size", &json!("huge")), None);
        assert_eq!(set_setting(&mut s, "tablo_width", &json!("auto")), Some(Effect::Tablo));
    }

    #[test]
    fn shortcut_is_mapped() {
        use crate::tab::{Cmd, map_key};
        assert_eq!(map_key('O' as u32, true, true, false), Some(Cmd::Bookmarks));
        assert_eq!(map_key('O' as u32, true, false, false), None);
    }

    #[test]
    fn pages_get_their_data() {
        let tiles = vec![Tile { url: "https://a.org/".into(), title: "A".into(), host: "a.org".into(), icon: String::new() }];
        let items = vec![Bookmark { url: "https://a.org/".into(), title: "</script>A".into(), added: 1 }];
        let html = bookmarks_page(&items, &tiles, "tok123");
        assert!(html.contains("tok123") && !html.contains("/*DATA*/") && !html.contains("</script>A"));
        let html = settings_page(&Settings::default(), "tok456");
        assert!(html.contains("tok456") && html.contains("\"search\":\"google\"") && !html.contains("live_blur"));
        assert_ne!(new_token(), new_token());
    }
}
