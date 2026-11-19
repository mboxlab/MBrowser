//! Ad & tracker blocker on Brave's adblock-rust engine.
//!
//! - Network: every request (except media streams) is checked against
//!   uBlock Origin's lists, EasyList, EasyPrivacy and RU AdList; `$redirect`
//!   rules get uBO's harmless stand-ins (noop.js, google-ima.js…).
//! - Cosmetic: per-site hiding rules, procedural filters (`:has-text`,
//!   `:upward`, `:remove()`…) and uBO scriptlets (`+js(...)`, e.g. YouTube ads)
//!   are injected before page scripts run; generic `##.ad` rules are matched
//!   against the classes and ids that actually appear on the page.
//!
//! Lists are downloaded in the background every few days; the compiled engine
//! is cached on disk (flatbuffers), so startup only maps a file. uBO's scriptlets
//! are ES modules: a hidden webview turns them into adblock-rust resources
//! (see `RESOURCES_PAGE`), the same way Brave's packager does.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use adblock::{
    Engine, FilterSet,
    cosmetic_filter_cache::UrlSpecificResources,
    lists::ParseOptions,
    request::Request,
    resources::{PermissionMask, Resource},
};
use serde::{Deserialize, Serialize};

use crate::{net, store};

/// Scriptlets marked `requiresTrust` by uBO (e.g. `trusted-set-cookie`) may only be
/// used by trusted lists: uBO's own lists and the user's blocklist.txt.
const TRUSTED: PermissionMask = PermissionMask::from_bits(1);

/// Lists are refreshed this often.
pub const UPDATE_EVERY_SECS: u64 = 4 * 24 * 3600;

#[derive(Clone, Copy, PartialEq)]
enum Group {
    Core,
    Cookies,
    Annoyances,
}

struct ListDef {
    id: &'static str,
    url: &'static str,
    trusted: bool,
    group: Group,
}

const UBO: &str = "https://ublockorigin.github.io/uAssetsCDN";
const LISTS: &[ListDef] = &[
    ListDef { id: "ubo-filters", url: "{UBO}/filters/filters.min.txt", trusted: true, group: Group::Core },
    ListDef { id: "ubo-badware", url: "{UBO}/filters/badware.min.txt", trusted: true, group: Group::Core },
    ListDef { id: "ubo-privacy", url: "{UBO}/filters/privacy.min.txt", trusted: true, group: Group::Core },
    ListDef { id: "ubo-unbreak", url: "{UBO}/filters/unbreak.min.txt", trusted: true, group: Group::Core },
    ListDef { id: "ubo-quick-fixes", url: "{UBO}/filters/quick-fixes.min.txt", trusted: true, group: Group::Core },
    ListDef { id: "easylist", url: "{UBO}/thirdparties/easylist.txt", trusted: false, group: Group::Core },
    ListDef { id: "easyprivacy", url: "{UBO}/thirdparties/easyprivacy.txt", trusted: false, group: Group::Core },
    ListDef {
        id: "ruadlist",
        url: "https://raw.githubusercontent.com/dimisa-RUAdList/RUAdListCDN/main/lists/ruadlist.ubo.min.txt",
        trusted: false,
        group: Group::Core,
    },
    ListDef { id: "easylist-cookies", url: "{UBO}/thirdparties/easylist-cookies.txt", trusted: false, group: Group::Cookies },
    ListDef { id: "ubo-cookies", url: "{UBO}/filters/annoyances-cookies.txt", trusted: true, group: Group::Cookies },
    ListDef {
        id: "easylist-annoyances",
        url: "{UBO}/thirdparties/easylist-annoyances.txt",
        trusted: false,
        group: Group::Annoyances,
    },
    ListDef { id: "ubo-annoyances", url: "{UBO}/filters/annoyances.min.txt", trusted: true, group: Group::Annoyances },
];

/// A safety net while the real lists are not downloaded yet (first start offline).
/// Domains are blocked as third parties; `host/path` rules everywhere.
const BUILTIN: &str = "
doubleclick.net googlesyndication.com googleadservices.com google-analytics.com
googletagmanager.com googletagservices.com adservice.google.com 2mdn.net admob.com
amazon-adsystem.com bat.bing.com clarity.ms pixel.facebook.com an.facebook.com
ads.yahoo.com analytics.yahoo.com advertising.com adtechus.com
adnxs.com adsrvr.org criteo.com criteo.net taboola.com outbrain.com pubmatic.com
rubiconproject.com openx.net casalemedia.com indexww.com smartadserver.com adform.net
moatads.com scorecardresearch.com quantserve.com quantcount.com chartbeat.com chartbeat.net
hotjar.com mouseflow.com crazyegg.com fullstory.com mixpanel.com cdn.segment.com api.segment.io
api.amplitude.com api2.amplitude.com nr-data.net adroll.com bidswitch.net bluekai.com
demdex.net everesttech.net krxd.net exelator.com agkn.com mathtag.com mookie1.com
rlcdn.com tapad.com adsymptotic.com contextweb.com sharethrough.com teads.tv triplelift.com
3lift.com yieldmo.com gumgum.com media.net zedo.com revcontent.com mgid.com propellerads.com
popads.net popcash.net adcash.com exoclick.com juicyads.com trafficjunky.net trafficjunky.com
adsterra.com hilltopads.net a-ads.com coinzilla.io bidvertiser.com infolinks.com viglink.com
skimresources.com sovrn.com lijit.com 33across.com undertone.com spotxchange.com spotx.tv
springserve.com serving-sys.com flashtalking.com innovid.com doubleverify.com
adsafeprotected.com adzerk.net omtrdc.net 2o7.net hs-analytics.net snap.licdn.com
analytics.twitter.com ads-twitter.com ct.pinterest.com analytics.tiktok.com tr.snapchat.com
statcounter.com histats.com yieldlab.net adition.com smartclip.net stickyadstv.com
adskeeper.com weborama.fr weborama.com gemius.pl
mc.yandex.ru mc.yandex.com an.yandex.ru yabs.yandex.ru yandexadexchange.net adfox.ru
adfox.yandex.ru awaps.yandex.net mc.webvisor.org webvisor.com top-fwz1.mail.ru ad.mail.ru
adriver.ru soloway.ru betweendigital.com buzzoola.com relap.io smi2.ru smi2.net
lentainform.com marketgid.com directadvert.ru otm-r.com counter.yadro.ru yadro.ru
liveinternet.ru tns-counter.ru counter.rambler.ru ssp.rambler.ru ssp01.rambler.ru
dsp-rambler.ru kraken.rambler.ru pixel.rambler.ru mpx.rambler.ru mpx2.rambler.ru
imgstats.rambler.ru top100.ru bumlam.com al-adtech.com ad-tech.ru
yandex.ru/ads/ yandex.ru/an/ facebook.com/tr? facebook.com/tr/ google.com/pagead/
youtube.com/pagead/ youtube.com/ptracking youtube.com/api/stats/ads vk.com/rtrg
youtube.com/api/stats/qoe youtube.com/api/stats/atr youtube.com/api/stats/delayplay
youtube.com/youtubei/v1/log_event youtube.com/get_midroll_info play.google.com/log
";

// ---------------------------------------------------------------- configuration

/// Which lists go into the engine (from settings).
#[derive(Clone, PartialEq)]
pub struct Config {
    pub cookies: bool,
    pub annoyances: bool,
    /// Extra list URLs added by the user in settings.json (untrusted).
    pub extra: Vec<String>,
}

struct ListSrc {
    file: String,
    url: String,
    trusted: bool,
}

impl Config {
    fn lists(&self) -> Vec<ListSrc> {
        let mut out: Vec<ListSrc> = LISTS
            .iter()
            .filter(|l| match l.group {
                Group::Core => true,
                Group::Cookies => self.cookies,
                Group::Annoyances => self.annoyances,
            })
            .map(|l| ListSrc { file: l.id.into(), url: l.url.replace("{UBO}", UBO), trusted: l.trusted })
            .collect();
        for url in &self.extra {
            if url.starts_with("https://") || url.starts_with("http://") {
                out.push(ListSrc { file: format!("extra-{:016x}", fnv(url)), url: url.clone(), trusted: false });
            }
        }
        out
    }

    /// Changes whenever the compiled engine would differ.
    fn signature(&self) -> String {
        let ids: Vec<String> = self.lists().into_iter().map(|l| l.file).collect();
        let user = fs::metadata(user_list_path())
            .ok()
            .map(|m| format!("{}-{}", m.len(), m.modified().ok().map(secs).unwrap_or(0)))
            .unwrap_or_default();
        format!("v1|{}|{user}", ids.join(","))
    }
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

pub fn dir() -> PathBuf {
    let d = store::cache_dir().join("Adblock");
    let _ = fs::create_dir_all(d.join("lists"));
    d
}

pub fn user_list_path() -> PathBuf {
    store::data_dir().join("blocklist.txt")
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct State {
    /// Signature of the cached engine.dat.
    sig: String,
    /// Last successful list download (unix seconds).
    fetched: u64,
}

fn read_state() -> State {
    fs::read(dir().join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_atomic(path: &Path, data: &[u8]) -> bool {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data).is_ok() && fs::rename(&tmp, path).is_ok()
}

/// What the background updater should do now, if anything.
pub enum Due {
    Nothing,
    /// Recompile from the lists on disk (settings or blocklist.txt changed).
    Rebuild,
    /// Download fresh lists, then recompile.
    Download,
}

pub fn due(cfg: &Config) -> Due {
    let st = read_state();
    let now = secs(SystemTime::now());
    let missing = cfg.lists().iter().any(|l| !dir().join("lists").join(format!("{}.txt", l.file)).exists());
    if now.saturating_sub(st.fetched) > UPDATE_EVERY_SECS || !dir().join("engine.dat").exists() {
        Due::Download
    } else if st.sig != cfg.signature() || missing {
        // A rebuild also fetches lists that are switched on but not on disk.
        Due::Rebuild
    } else {
        Due::Nothing
    }
}

/// Last successful download, unix seconds (0 = never).
pub fn last_fetched() -> u64 {
    read_state().fetched
}

// ---------------------------------------------------------------- engine life cycle

/// Startup: the cached engine (any version of the lists) plus resources.
/// Falls back to the built-in list; never touches the network.
pub fn load_cached() -> Engine {
    let mut engine = Engine::default();
    let ok = fs::read(dir().join("engine.dat")).ok().is_some_and(|b| engine.deserialize(&b).is_ok());
    if !ok {
        engine = compile(&[]);
    }
    apply_resources(&mut engine);
    engine
}

/// Downloads (optionally) and compiles. Runs in a background thread.
pub fn update(cfg: &Config, download: bool) -> Result<Engine, String> {
    let lists = cfg.lists();
    let mut st = read_state();
    let file = |l: &ListSrc| dir().join("lists").join(format!("{}.txt", l.file));
    // A full update fetches everything; a rebuild still fetches lists that were
    // just switched on in settings and are not on disk yet.
    let wanted: Vec<&ListSrc> = lists.iter().filter(|l| download || !file(l).exists()).collect();
    let mut got = 0;
    for l in &wanted {
        match net::get(&l.url) {
            Ok(r) if r.ok() && looks_like_list(&r.body) => {
                if write_atomic(&file(l), &r.body) {
                    got += 1;
                }
            }
            Ok(r) => eprintln!("adblock: {} -> HTTP {}", l.url, r.status),
            Err(e) => eprintln!("adblock: {} -> {e}", l.url),
        }
    }
    if download {
        if got == 0 {
            return Err("не удалось скачать списки фильтров".into());
        }
        st.fetched = secs(SystemTime::now());
    }
    let sources: Vec<(String, bool)> = lists
        .iter()
        .filter_map(|l| fs::read(dir().join("lists").join(format!("{}.txt", l.file))).ok().map(|b| (l, b)))
        .map(|(l, b)| (preprocess(&String::from_utf8_lossy(&b)), l.trusted))
        .collect();
    let mut engine = compile(&sources);
    write_atomic(&dir().join("engine.dat"), &engine.serialize());
    st.sig = cfg.signature();
    write_atomic(&dir().join("state.json"), &serde_json::to_vec(&st).unwrap_or_default());
    apply_resources(&mut engine);
    Ok(engine)
}

fn looks_like_list(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&body[..body.len().min(512)]).to_ascii_lowercase();
    body.len() > 200 && !head.contains("<html") && !head.contains("<!doctype")
}

/// Built-in rules + downloaded lists + the user's blocklist.txt.
fn compile(lists: &[(String, bool)]) -> Engine {
    let mut set = FilterSet::new(false);
    set.add_filter_list(builtin_rules().join("\n"), ParseOptions::default());
    for (text, trusted) in lists {
        let opts = ParseOptions { permissions: if *trusted { TRUSTED } else { PermissionMask::default() }, ..Default::default() };
        set.add_filter_list(text.clone(), opts);
    }
    if let Ok(text) = fs::read_to_string(user_list_path()) {
        let rules: Vec<String> = preprocess(&text).lines().filter_map(user_rule).collect();
        set.add_filter_list(rules.join("\n"), ParseOptions { permissions: TRUSTED, ..Default::default() });
    }
    Engine::new_with_filter_set(set)
}

fn builtin_rules() -> Vec<String> {
    BUILTIN
        .split_whitespace()
        .map(|r| if r.contains('/') { format!("||{r}") } else { format!("||{r}^$third-party") })
        .collect()
}

/// blocklist.txt accepts plain domains, `domain/path`, hosts-file lines and any
/// uBlock Origin / AdBlock Plus rule.
fn user_rule(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    let target = if first.parse::<std::net::IpAddr>().is_ok() {
        // hosts file: "0.0.0.0 ads.example.com  # comment"
        let host = parts.next()?;
        if matches!(host, "localhost" | "localhost.localdomain" | "local" | "broadcasthost" | "0.0.0.0") {
            return None;
        }
        host
    } else {
        line
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '?' | '=' | '&');
    let host = target.split('/').next().unwrap_or("");
    if target.chars().all(plain) && host.contains('.') && !host.starts_with('.') {
        let t = target.to_ascii_lowercase();
        return Some(if t.contains('/') { format!("||{t}") } else { format!("||{t}^") });
    }
    Some(line.to_string())
}

// ---------------------------------------------------------------- list preprocessor

/// Evaluates uBO's `!#if` / `!#else` / `!#endif` directives for a Chromium
/// browser that understands uBO syntax but not HTML filtering.
pub fn preprocess(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut stack: Vec<(bool, bool)> = Vec::new(); // (outer active, condition)
    let mut active = true;
    for line in text.lines() {
        if let Some(expr) = line.strip_prefix("!#if ") {
            let cond = eval_condition(expr);
            stack.push((active, cond));
            active = active && cond;
        } else if line.starts_with("!#else") {
            if let Some(&(outer, cond)) = stack.last() {
                active = outer && !cond;
            }
        } else if line.starts_with("!#endif") {
            if let Some((outer, _)) = stack.pop() {
                active = outer;
            }
        } else if active && !line.starts_with("!#") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn eval_condition(expr: &str) -> bool {
    let tokens: Vec<String> = {
        let mut t = Vec::new();
        let mut cur = String::new();
        let b = expr.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i] as char;
            if c.is_ascii_alphanumeric() || c == '_' {
                cur.push(c);
            } else {
                if !cur.is_empty() {
                    t.push(std::mem::take(&mut cur));
                }
                match c {
                    '!' | '(' | ')' => t.push(c.to_string()),
                    '&' | '|' if b.get(i + 1) == Some(&b[i]) => {
                        t.push(format!("{c}{c}"));
                        i += 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if !cur.is_empty() {
            t.push(cur);
        }
        t
    };
    let mut pos = 0;
    parse_or(&tokens, &mut pos)
}

fn parse_or(t: &[String], pos: &mut usize) -> bool {
    let mut v = parse_and(t, pos);
    while t.get(*pos).is_some_and(|s| s == "||") {
        *pos += 1;
        v |= parse_and(t, pos);
    }
    v
}

fn parse_and(t: &[String], pos: &mut usize) -> bool {
    let mut v = parse_unary(t, pos);
    while t.get(*pos).is_some_and(|s| s == "&&") {
        *pos += 1;
        v &= parse_unary(t, pos);
    }
    v
}

fn parse_unary(t: &[String], pos: &mut usize) -> bool {
    let Some(tok) = t.get(*pos) else { return false };
    *pos += 1;
    match tok.as_str() {
        "!" => !parse_unary(t, pos),
        "(" => {
            let v = parse_or(t, pos);
            if t.get(*pos).is_some_and(|s| s == ")") {
                *pos += 1;
            }
            v
        }
        name => matches!(name, "env_chromium" | "ext_ublock" | "cap_user_stylesheet"),
    }
}

// ---------------------------------------------------------------- resources (scriptlets, redirects)

/// Loads uBO's resources into the engine; returns how many were usable.
pub fn apply_resources(engine: &mut Engine) -> usize {
    let Some(list) = fs::read(dir().join("resources.json")).ok().and_then(|b| serde_json::from_slice::<Vec<Resource>>(&b).ok())
    else {
        return 0;
    };
    let n = list.len();
    engine.use_resources(list);
    n
}

/// Validates resources produced by `RESOURCES_PAGE` and stores them.
pub fn save_resources(json: &str) -> Option<Vec<Resource>> {
    let list: Vec<Resource> = serde_json::from_str(json).ok()?;
    if list.len() < 50 {
        return None; // the import half failed; keep the previous set
    }
    write_atomic(&dir().join("resources.json"), json.as_bytes()).then_some(list)
}

/// Runs in a hidden, unprivileged webview: imports uBlock Origin's scriptlet
/// modules and redirect resources from its latest release and posts them back as
/// adblock-rust `Resource`s (scriptlets become `fn/javascript` functions with
/// dependencies; `requiresTrust` becomes the TRUSTED permission bit).
pub const RESOURCES_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><script type="module">
const BASE = 'https://cdn.jsdelivr.net/gh/gorhill/uBlock/src/';
const b64 = s => { const u = new TextEncoder().encode(s); let b = ''; for (let i = 0; i < u.length; i += 8192) b += String.fromCharCode(...u.subarray(i, i + 8192)); return btoa(b); };
const b64buf = buf => { const u = new Uint8Array(buf); let b = ''; for (let i = 0; i < u.length; i += 8192) b += String.fromCharCode(...u.subarray(i, i + 8192)); return btoa(b); };
const MIME = { js: 'application/javascript', css: 'text/css', html: 'text/html', json: 'application/json', txt: 'text/plain', xml: 'text/xml', gif: 'image/gif', png: 'image/png', mp3: 'audio/mp3', mp4: 'video/mp4' };
const out = [];
try {
  const { builtinScriptlets } = await import(BASE + 'js/resources/scriptlets.js');
  for (const s of builtinScriptlets) {
    if (!s || typeof s.fn !== 'function' || !s.name) continue;
    const fn = s.name.endsWith('.fn');
    out.push({
      name: s.name,
      aliases: Array.isArray(s.aliases) ? s.aliases : [],
      kind: { mime: fn ? 'fn/javascript' : 'application/javascript' },
      content: b64(s.fn.toString()),
      dependencies: (s.dependencies || []).filter(d => typeof d === 'string'),
      permission: s.requiresTrust ? 1 : 0,
    });
  }
} catch (e) { window.ipc.postMessage(JSON.stringify({ error: 'scriptlets: ' + e })); }
try {
  const { default: map } = await import(BASE + 'js/redirect-resources.js');
  const jobs = [];
  for (const [name, d] of map) {
    if (d && d.params) continue;
    const ext = name.split('.').pop();
    if (!MIME[ext]) continue;
    jobs.push(fetch(BASE + 'web_accessible_resources/' + name).then(r => r.ok ? r.arrayBuffer() : null).then(buf => {
      if (!buf) return;
      const alias = d && d.alias ? (Array.isArray(d.alias) ? d.alias : [d.alias]) : [];
      out.push({ name, aliases: alias, kind: { mime: MIME[ext] }, content: b64buf(buf) });
    }).catch(() => {}));
  }
  await Promise.all(jobs);
} catch (e) {}
window.ipc.postMessage(JSON.stringify({ resources: out }));
</script>"#;

// ---------------------------------------------------------------- runtime

/// Shared blocker state, read by every tab's hooks (all on the UI thread).
pub struct BlockCtx {
    pub engine: RefCell<Engine>,
    pub enabled: Cell<bool>,
    pub exceptions: RefCell<HashSet<String>>,
    /// SponsorBlock on YouTube (part of the same page script).
    pub sponsorblock: Cell<bool>,
    /// Recently computed cosmetic data per host (cleared when the engine changes).
    cosmetic: RefCell<HashMap<String, std::rc::Rc<Cosmetic>>>,
}

/// What a page needs at document start.
pub struct Cosmetic {
    /// JSON for the content script: hide selectors, procedural filters, generic flag.
    pub json: String,
    /// uBO scriptlets for this site (JS code).
    pub scriptlets: String,
    /// Selectors excluded from generic hiding on this site.
    pub exceptions: HashSet<String>,
}

pub enum Verdict {
    Allow,
    Block,
    /// Serve this instead: (content type, body).
    Redirect(String, Vec<u8>),
}

impl BlockCtx {
    pub fn new(engine: Engine, enabled: bool, exceptions: HashSet<String>) -> Self {
        Self {
            engine: RefCell::new(engine),
            enabled: Cell::new(enabled),
            exceptions: RefCell::new(exceptions),
            sponsorblock: Cell::new(false),
            cosmetic: RefCell::new(HashMap::new()),
        }
    }

    pub fn set_engine(&self, engine: Engine) {
        *self.engine.borrow_mut() = engine;
        self.cosmetic.borrow_mut().clear();
    }

    pub fn with_engine<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let r = f(&mut self.engine.borrow_mut());
        self.cosmetic.borrow_mut().clear();
        r
    }

    pub fn is_exception(&self, host: &str) -> bool {
        let ex = self.exceptions.borrow();
        let mut h = site_key(host);
        loop {
            if ex.contains(h) {
                return true;
            }
            match h.find('.') {
                Some(i) if h[i + 1..].contains('.') => h = &h[i + 1..],
                _ => return false,
            }
        }
    }

    /// `kind` is an adblock request type ("script", "image", "subdocument"…).
    pub fn check(&self, url: &str, page_url: &str, kind: &str, method: &str) -> Verdict {
        let Ok(engine) = self.engine.try_borrow() else { return Verdict::Allow };
        let Ok(req) = Request::new(url, page_url, kind, method) else { return Verdict::Allow };
        let res = engine.check_network_request(&req);
        if !res.should_block() {
            return Verdict::Allow;
        }
        match res.redirect.as_deref().and_then(decode_data_url) {
            Some((mime, body)) => Verdict::Redirect(mime, body),
            None => Verdict::Block,
        }
    }

    /// Cosmetic resources for a page (cached per host).
    pub fn cosmetic(&self, url: &str) -> Option<std::rc::Rc<Cosmetic>> {
        let host = host_of(url)?.to_ascii_lowercase();
        if let Some(c) = self.cosmetic.borrow().get(&host) {
            return Some(c.clone());
        }
        let r: UrlSpecificResources = self.engine.try_borrow().ok()?.url_cosmetic_resources(url);
        let mut hide: Vec<&String> = r.hide_selectors.iter().collect();
        hide.sort_unstable();
        let mut proc: Vec<&String> = r.procedural_actions.iter().collect();
        proc.sort_unstable();
        let json = serde_json::json!({ "hide": hide, "proc": proc, "generic": !r.generichide }).to_string();
        let c = std::rc::Rc::new(Cosmetic { json, scriptlets: r.injected_script, exceptions: r.exceptions });
        let mut cache = self.cosmetic.borrow_mut();
        if cache.len() > 16 {
            cache.clear();
        }
        cache.insert(host, c.clone());
        Some(c)
    }

    /// Generic `##.class` / `##[id]` rules for names seen on a page.
    pub fn generic(&self, classes: &[&str], ids: &[&str], exceptions: &HashSet<String>) -> Vec<String> {
        self.engine
            .try_borrow()
            .map(|e| e.hidden_class_id_selectors(classes.iter(), ids.iter(), exceptions))
            .unwrap_or_default()
    }
}

/// "data:application/javascript;base64,AAAA" -> (mime, bytes)
fn decode_data_url(u: &str) -> Option<(String, Vec<u8>)> {
    let rest = u.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime.to_string(), base64_decode(data)?))
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- URL helpers

/// Host part of an http(s) URL, without port and credentials.
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..end];
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    if auth.starts_with('[') {
        return auth.find(']').map(|i| &auth[..=i]);
    }
    let host = auth.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// "www.example.com" -> "example.com"
pub fn site_key(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with(rules: &str) -> Engine {
        compile(&[(preprocess(rules), true)])
    }

    #[test]
    fn hosts_and_rules() {
        assert_eq!(host_of("https://user:pw@Sub.Example.com:8080/x?y#z"), Some("Sub.Example.com"));
        assert_eq!(host_of("http://a.b/"), Some("a.b"));
        assert_eq!(host_of("data:text/html,hi"), None);
        assert_eq!(user_rule("0.0.0.0 ads.example.com # c"), Some("||ads.example.com^".into()));
        assert_eq!(user_rule("127.0.0.1 localhost"), None);
        assert_eq!(user_rule("Example.com/ads/"), Some("||example.com/ads/".into()));
        assert_eq!(user_rule("||track.example.org^"), Some("||track.example.org^".into()));
        assert_eq!(user_rule("example.org##.banner"), Some("example.org##.banner".into()));
        assert_eq!(user_rule("# comment"), None);
    }

    #[test]
    fn preprocessor() {
        let text = "a\n!#if env_firefox\nb\n!#else\nc\n!#endif\n!#if !cap_html_filtering\nd\n!#if env_mobile\ne\n!#endif\n!#endif\n\
                    !#if (ext_ublock || adguard) && !env_safari\nf\n!#endif\n!#include x.txt\ng\n";
        assert_eq!(preprocess(text), "a\nc\nd\nf\ng\n");
    }

    #[test]
    fn builtin_blocking() {
        let ctx = BlockCtx::new(compile(&[]), true, HashSet::new());
        let page = "https://www.example.com/";
        assert!(matches!(ctx.check("https://securepubads.g.doubleclick.net/tag/js/gpt.js", page, "script", "get"), Verdict::Block));
        assert!(matches!(ctx.check("https://mc.yandex.ru/metrika/tag.js", page, "script", "get"), Verdict::Block));
        // First party: a site's own analytics domain is not cut by the safety net.
        assert!(matches!(ctx.check("https://mc.yandex.ru/watch/1", "https://yandex.ru/", "image", "get"), Verdict::Allow));
        assert!(matches!(ctx.check("https://yandex.ru/ads/system/context.js", "https://yandex.ru/", "script", "get"), Verdict::Block));
        assert!(matches!(ctx.check("https://www.youtube.com/api/stats/qoe?docid=x", "https://www.youtube.com/watch?v=x", "xmlhttprequest", "post"), Verdict::Block));
        assert!(matches!(ctx.check("https://www.youtube.com/watch?v=x", page, "subdocument", "get"), Verdict::Allow));
        assert!(matches!(ctx.check("https://notdoubleclick.net/x.js", page, "script", "get"), Verdict::Allow));
    }

    #[test]
    fn abp_syntax_and_cosmetics() {
        let e = engine_with(
            "||ads.example.net^$script\n@@||ads.example.net/ok.js\n/banner/*$image\n\
             example.com##.promo\n##.ad-slot\nexample.com##div:has-text(Реклама)\n!#if env_firefox\n||ff-only.net^\n!#endif\n",
        );
        let ctx = BlockCtx::new(e, true, HashSet::new());
        let page = "https://example.com/";
        assert!(matches!(ctx.check("https://ads.example.net/a.js", page, "script", "get"), Verdict::Block));
        assert!(matches!(ctx.check("https://ads.example.net/ok.js", page, "script", "get"), Verdict::Allow));
        assert!(matches!(ctx.check("https://ads.example.net/a.png", page, "image", "get"), Verdict::Allow));
        assert!(matches!(ctx.check("https://cdn.site.org/banner/1.png", page, "image", "get"), Verdict::Block));
        assert!(matches!(ctx.check("https://ff-only.net/x.js", page, "script", "get"), Verdict::Allow));
        let c = ctx.cosmetic("https://example.com/page").unwrap();
        assert!(c.json.contains(".promo"), "{}", c.json);
        assert!(c.json.contains("has-text"), "{}", c.json);
        assert!(!ctx.cosmetic("https://other.org/").unwrap().json.contains(".promo"));
        let generic = ctx.generic(&["ad-slot", "content"], &[], &c.exceptions);
        assert_eq!(generic, vec![".ad-slot".to_string()]);
    }

    #[test]
    fn data_urls() {
        assert_eq!(decode_data_url("data:text/plain;base64,aGk="), Some(("text/plain".into(), b"hi".to_vec())));
        assert_eq!(base64_decode("Zm9vYmFy"), Some(b"foobar".to_vec()));
    }

    /// What a site gets from a compiled engine + resources in `MB_ADBLOCK=<dir>`:
    /// `cargo test site_resources -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn site_resources() {
        let dir = PathBuf::from(std::env::var("MB_ADBLOCK").expect("MB_ADBLOCK"));
        let mut e = Engine::default();
        e.deserialize(&fs::read(dir.join("engine.dat")).unwrap()).unwrap();
        let res: Vec<Resource> = serde_json::from_slice(&fs::read(dir.join("resources.json")).unwrap()).unwrap();
        eprintln!("resources: {}", res.len());
        e.use_resources(res);
        let url = std::env::var("MB_URL").unwrap_or_else(|_| "https://www.youtube.com/watch?v=x".into());
        let r = e.url_cosmetic_resources(&url);
        let calls: Vec<&str> =
            r.injected_script.lines().filter(|l| !l.starts_with("function") && l.contains('(') && !l.starts_with(' ')).collect();
        eprintln!("hide {} proc {} script {} bytes", r.hide_selectors.len(), r.procedural_actions.len(), r.injected_script.len());
        let filter = std::env::var("MB_GREP").unwrap_or_default();
        for s in r.hide_selectors.iter().chain(r.procedural_actions.iter()).filter(|s| s.contains(filter.as_str())) {
            eprintln!("  sel {s}");
        }
        eprintln!("invocations:");
        for c in calls.iter().filter(|c| !c.starts_with("try") && !c.starts_with('}')) {
            eprintln!("  {}", c.chars().take(200).collect::<String>());
        }
    }

    /// Real lists from a local folder: `MB_LISTS=<dir> cargo test real_lists -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_lists() {
        let dir = PathBuf::from(std::env::var("MB_LISTS").expect("MB_LISTS"));
        let t = std::time::Instant::now();
        let mut sources = Vec::new();
        for l in LISTS.iter().filter(|l| l.group == Group::Core) {
            let name = match l.id {
                "ubo-quick-fixes" => "ubo-quick".to_string(),
                id => id.to_string(),
            };
            if let Ok(text) = fs::read_to_string(dir.join(format!("{name}.txt"))) {
                sources.push((preprocess(&text), l.trusted));
            }
        }
        let read = t.elapsed();
        let engine = compile(&sources);
        let built = t.elapsed();
        let bytes = engine.serialize();
        let mut e2 = Engine::default();
        let t2 = std::time::Instant::now();
        e2.deserialize(&bytes).unwrap();
        eprintln!(
            "lists: {} read {read:?}, compiled {:?}, engine.dat {} KB, deserialize {:?}",
            sources.len(),
            built - read,
            bytes.len() / 1024,
            t2.elapsed()
        );
        let ctx = BlockCtx::new(e2, true, HashSet::new());
        let t3 = std::time::Instant::now();
        let n = 2000;
        for i in 0..n {
            let _ = ctx.check(&format!("https://cdn.example.com/static/app{i}.js"), "https://example.com/", "script", "get");
        }
        eprintln!("check: {:?} per request", t3.elapsed() / n);
        assert!(matches!(
            ctx.check("https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js", "https://example.com/", "script", "get"),
            Verdict::Block | Verdict::Redirect(..)
        ));
        let t4 = std::time::Instant::now();
        let c = ctx.cosmetic("https://www.youtube.com/watch?v=x").unwrap();
        eprintln!("cosmetic youtube: {} bytes json, {} bytes scriptlets, {:?}", c.json.len(), c.scriptlets.len(), t4.elapsed());
    }
}
