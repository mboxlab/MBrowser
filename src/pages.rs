//! Built-in pages: new tab (speed dial) and history. Generated on the fly,
//! data is embedded, no network or IPC needed.

use serde::Serialize;

use crate::blocker::{host_of, site_key};
use crate::store::{History, TabloItem};

/// JSON safe to embed inside <script>.
fn embed<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into()).replace("</", "<\\/")
}

#[derive(Serialize)]
pub struct Tile {
    pub url: String,
    pub title: String,
    pub host: String,
    /// Large icon learned from the site itself ("" = unknown).
    pub icon: String,
}

/// Short human name for a site: "www.youtube.com" -> "Youtube", "127.0.0.1" stays.
pub fn site_label(host: &str) -> String {
    let h = site_key(host);
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2 || h.parse::<std::net::IpAddr>().is_ok() {
        return h.to_string();
    }
    let name = labels[labels.len() - 2];
    let mut c = name.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => h.to_string(),
    }
}

/// Sites from history, one per host: most visited (`recent == false`) or most recent.
pub fn history_sites(history: &History, limit: usize, recent: bool) -> Vec<Tile> {
    let mut per_host: std::collections::HashMap<&str, (u32, u64, &str)> = Default::default();
    for e in &history.entries {
        let Some(h) = host_of(&e.url) else { continue };
        let slot = per_host.entry(h).or_insert((0, 0, &e.url));
        slot.0 += e.visits;
        slot.1 = slot.1.max(e.last);
        if e.url.len() < slot.2.len() {
            slot.2 = &e.url;
        }
    }
    let mut hosts: Vec<_> = per_host.into_iter().collect();
    if recent {
        hosts.sort_unstable_by(|a, b| b.1.1.cmp(&a.1.1));
    } else {
        hosts.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0));
    }
    let mut seen = std::collections::HashSet::new();
    hosts
        .into_iter()
        .filter(|(h, _)| seen.insert(site_key(h).to_string()))
        .take(limit)
        .map(|(h, (_, _, url))| {
            let root = url.find(h).map(|i| format!("{}/", &url[..i + h.len()])).unwrap_or_else(|| url.to_string());
            Tile { url: root, title: site_label(h), host: site_key(h).to_string(), icon: String::new() }
        })
        .collect()
}

/// Tablo items as tiles (host added for icons).
pub fn tablo_tiles(items: &[TabloItem]) -> Vec<Tile> {
    items
        .iter()
        .map(|t| Tile {
            url: t.url.clone(),
            title: t.title.clone(),
            host: host_of(&t.url).map(|h| h.to_string()).unwrap_or_default(),
            icon: String::new(),
        })
        .collect()
}

/// The "new tab" page shows the same Tablo as the launcher (read-only here: tiles
/// are managed from "+"). Private tabs get a one-line note instead of tiles.
pub fn newtab(tiles: &[Tile], private: bool) -> String {
    let tiles: &[Tile] = if private { &[] } else { tiles };
    NEWTAB
        .replace("/*TILES*/null", &embed(&tiles))
        .replace("/*PRIVATE*/false", if private { "true" } else { "false" })
}

#[derive(Serialize)]
struct Row<'a> {
    u: &'a str,
    t: &'a str,
    l: u64,
}

pub fn history(history: &History) -> String {
    let mut rows: Vec<Row> =
        history.entries.iter().map(|e| Row { u: &e.url, t: &e.title, l: e.last }).collect();
    rows.sort_unstable_by(|a, b| b.l.cmp(&a.l));
    rows.truncate(4000);
    HISTORY.replace("/*ROWS*/null", &embed(&rows))
}

const NEWTAB: &str = r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><title>Новая вкладка</title>
<style>
*{box-sizing:border-box}html,body{margin:0;min-height:100%}
body{background:#111216;color:#e9e9ee;font:15px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif;padding:7vh 24px 40px}
#grid{display:grid;grid-template-columns:repeat(auto-fit,240px);justify-content:center;gap:10px;width:min(1020px,100%);margin:0 auto}
a.tile{height:146px;border-radius:14px;background:#fff;color:#1d1d22;text-decoration:none;display:flex;flex-direction:column;
  align-items:center;justify-content:center;gap:14px;transition:transform .12s ease,box-shadow .12s ease}
a.tile:hover{transform:translateY(-2px);box-shadow:0 10px 26px #0006}
.tic{width:64px;height:64px;display:grid;place-items:center;border-radius:16px;background:#f0f1f4;color:#4a4c57;font:600 26px "Segoe UI",sans-serif;overflow:hidden}
.tic img{width:64px;height:64px;object-fit:contain;border-radius:14px}
.tic img.small{width:32px;height:32px;border-radius:6px}
.tnm{font-size:15px;max-width:calc(100% - 24px);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#priv{display:none;max-width:520px;margin:18vh auto 0;text-align:center;color:#9a9ca6;line-height:1.6}
#priv b{display:block;color:#e9e9ee;font-weight:600;margin-bottom:6px}
body.private{background:#16121f}body.private #priv{display:block}
</style></head><body>
<div id="grid"></div>
<div id="priv"><b>Приватная вкладка</b>История не сохраняется. Cookies и данные сайтов удалятся, когда вы закроете все приватные вкладки.</div>
<script>
const TILES=/*TILES*/null, PRIV=/*PRIVATE*/false;
if(PRIV)document.body.classList.add('private');
function icon(t){
  const box=document.createElement('div');box.className='tic';
  const l=document.createElement('span');l.textContent=(t.title||t.host||'?').trim().charAt(0).toUpperCase();box.appendChild(l);
  if(t.icon&&t.icon.startsWith('data:')){const im=new Image();im.onload=()=>{im.className=im.naturalWidth<48?'small':''};im.src=t.icon;l.remove();box.appendChild(im);return box}
  if(!t.host)return box;
  const ip=/^[\d.:[\]]+$/.test(t.host), tries=[];
  if(t.icon)tries.push(t.icon);
  if(!ip)tries.push('https://favicon.yandex.net/favicon/v2/'+t.host+'?size=120');
  tries.push((ip?'http':'https')+'://'+t.host+'/favicon.ico');
  const img=new Image();
  img.onload=()=>{if(img.naturalWidth<2)return img.onerror();img.className=img.naturalWidth<48?'small':'';l.remove();box.appendChild(img)};
  img.onerror=()=>{const n=tries.shift();if(n)img.src=n};
  addEventListener('load',()=>img.onerror());
  return box;
}
const grid=document.getElementById('grid');
for(const t of TILES||[]){
  const a=document.createElement('a');a.className='tile';a.href=t.url;a.title=t.url;
  const nm=document.createElement('div');nm.className='tnm';nm.textContent=t.title||t.host;
  a.append(icon(t),nm);grid.appendChild(a);
}
</script></body></html>"#;

const HISTORY: &str = r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><title>История</title>
<style>
:root{--bg:#111216;--fg:#e9e9ee;--mut:#8b8d98;--card:#1b1c22;--acc:#8a96ff}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif}
main{max-width:860px;margin:0 auto;padding:32px 16px 64px}
h1{font:600 26px "Segoe UI Variable Display","Segoe UI",system-ui;margin:0 0 18px}
input{width:100%;height:44px;border-radius:22px;border:1px solid #ffffff14;background:var(--card);color:var(--fg);font:inherit;font-size:15px;padding:0 20px;outline:none;margin-bottom:18px}
input:focus{border-color:var(--acc)}
h2{font-size:13px;font-weight:600;color:var(--mut);margin:22px 0 6px}h2::first-letter{text-transform:uppercase}
a{display:flex;gap:14px;align-items:center;padding:8px 12px;border-radius:10px;color:var(--fg);text-decoration:none}
a:hover{background:var(--card)}
.tm{color:var(--mut);font-variant-numeric:tabular-nums;width:40px;flex:none;font-size:12px}
.tt{flex:1;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hs{color:var(--mut);font-size:12px;max-width:40%;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#more{display:none;margin:16px auto;background:var(--card);color:var(--fg);border:0;border-radius:18px;padding:9px 18px;font:inherit;cursor:pointer}
.empty{color:var(--mut);padding:24px 12px}
</style></head><body><main>
<h1>История</h1><input id="q" autofocus placeholder="Поиск по истории" spellcheck="false">
<div id="list"></div><button id="more">Показать ещё</button>
</main><script>
const ROWS=/*ROWS*/null||[];
const list=document.getElementById('list'),more=document.getElementById('more');
let limit=300;
function host(u){try{return new URL(u).host}catch{return u}}
function render(){
  const q=document.getElementById('q').value.toLowerCase().split(/\s+/).filter(Boolean);
  const rows=q.length?ROWS.filter(r=>{const s=(r.t+' '+r.u).toLowerCase();return q.every(w=>s.includes(w))}):ROWS;
  list.textContent='';let day='';const frag=document.createDocumentFragment();
  for(const r of rows.slice(0,limit)){
    const d=new Date(r.l*1000),dl=d.toLocaleDateString('ru-RU',{weekday:'long',day:'numeric',month:'long'});
    if(dl!==day){day=dl;const h=document.createElement('h2');h.textContent=dl;frag.appendChild(h)}
    const a=document.createElement('a');a.href=r.u;
    const tm=document.createElement('span');tm.className='tm';tm.textContent=d.toLocaleTimeString('ru-RU',{hour:'2-digit',minute:'2-digit'});
    const tt=document.createElement('span');tt.className='tt';tt.textContent=r.t||r.u;
    const hs=document.createElement('span');hs.className='hs';hs.textContent=host(r.u);
    a.append(tm,tt,hs);frag.appendChild(a);
  }
  if(!rows.length){const e=document.createElement('div');e.className='empty';e.textContent='Ничего не найдено';frag.appendChild(e)}
  list.appendChild(frag);more.style.display=rows.length>limit?'block':'none';
}
document.getElementById('q').oninput=()=>{limit=300;render()};
more.onclick=()=>{limit+=500;render()};
render();
</script></body></html>"#;
