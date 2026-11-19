"""Общий код переноса данных из браузеров на Chromium в MBrowser.

Отдельные скрипты (import_vivaldi.py, import_yandex.py) добавляют к нему то, что
у каждого браузера своё: формат паролей, чтение открытых вкладок и Экспресс-панели.
Переносятся: вкладки по пространствам, пароли, cookies (сайты остаются залогинены),
закладки, плитки Экспресс-панели (в Табло), история, включённые расширения с данными.

Только Windows и стандартная библиотека Python 3.8+: ключи расшифровываются через
DPAPI, AES-GCM — через системный BCrypt.
"""

import ctypes
import hashlib
import os
import shutil
import sqlite3
import time
from base64 import b64decode
from ctypes import wintypes
from pathlib import Path

CHROME_EPOCH = 11644473600  # секунд между 1601-01-01 и 1970-01-01
HISTORY_KEEP = 15_000       # как HISTORY_KEEP в src/store.rs
SPACE_COLORS = ["#58b4f0", "#5ec8a0", "#f0a35e", "#ef6f8f", "#8a96ff", "#c48cff", "#e8d25a"]

# ------------------------------------------------------------------ crypto (DPAPI + BCrypt AES-GCM)


class _Blob(ctypes.Structure):
    _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.c_void_p)]


def dpapi_unprotect(data: bytes) -> bytes:
    crypt32, kernel32 = ctypes.windll.crypt32, ctypes.windll.kernel32
    kernel32.LocalFree.argtypes = [ctypes.c_void_p]
    buf = ctypes.create_string_buffer(data, len(data))
    src, out = _Blob(len(data), ctypes.cast(buf, ctypes.c_void_p)), _Blob()
    if not crypt32.CryptUnprotectData(ctypes.byref(src), None, None, None, None, 0, ctypes.byref(out)):
        raise ctypes.WinError()
    try:
        return ctypes.string_at(out.pbData, out.cbData)
    finally:
        kernel32.LocalFree(out.pbData)


class _AuthInfo(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.ULONG), ("dwInfoVersion", wintypes.ULONG),
        ("pbNonce", ctypes.c_void_p), ("cbNonce", wintypes.ULONG),
        ("pbAuthData", ctypes.c_void_p), ("cbAuthData", wintypes.ULONG),
        ("pbTag", ctypes.c_void_p), ("cbTag", wintypes.ULONG),
        ("pbMacContext", ctypes.c_void_p), ("cbMacContext", wintypes.ULONG),
        ("cbAAD", wintypes.ULONG), ("cbData", ctypes.c_ulonglong), ("dwFlags", wintypes.ULONG),
    ]


_bcrypt = ctypes.WinDLL("bcrypt")
for _fn in ("BCryptEncrypt", "BCryptDecrypt"):
    getattr(_bcrypt, _fn).argtypes = [
        ctypes.c_void_p, ctypes.c_void_p, wintypes.ULONG, ctypes.c_void_p, ctypes.c_void_p,
        wintypes.ULONG, ctypes.c_void_p, wintypes.ULONG, ctypes.POINTER(wintypes.ULONG), wintypes.ULONG,
    ]
    getattr(_bcrypt, _fn).restype = ctypes.c_long
_bcrypt.BCryptOpenAlgorithmProvider.argtypes = [ctypes.POINTER(ctypes.c_void_p), wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.ULONG]
_bcrypt.BCryptSetProperty.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR, ctypes.c_void_p, wintypes.ULONG, wintypes.ULONG]
_bcrypt.BCryptGenerateSymmetricKey.argtypes = [
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p, wintypes.ULONG,
    ctypes.c_void_p, wintypes.ULONG, wintypes.ULONG,
]


def _ok(status):
    if status != 0:
        raise OSError(f"BCrypt NTSTATUS {status & 0xFFFFFFFF:#010x}")


class AesGcm:
    """Ключ AES-GCM поверх системного BCrypt. Из сырого ключа (from_raw) или из
    ключа os_crypt профиля Chromium (from_local_state): формат v10 = 'v10' +
    nonce(12) + шифртекст + тег(16)."""

    def __init__(self, key: bytes):
        self._alg, self._key = ctypes.c_void_p(), ctypes.c_void_p()
        _ok(_bcrypt.BCryptOpenAlgorithmProvider(ctypes.byref(self._alg), "AES", None, 0))
        mode = ctypes.create_unicode_buffer("ChainingModeGCM")
        _ok(_bcrypt.BCryptSetProperty(self._alg, "ChainingMode", mode, ctypes.sizeof(mode), 0))
        self._kbuf = ctypes.create_string_buffer(key, len(key))
        _ok(_bcrypt.BCryptGenerateSymmetricKey(self._alg, ctypes.byref(self._key), None, 0, self._kbuf, len(key), 0))

    @classmethod
    def from_raw(cls, key: bytes) -> "AesGcm":
        return cls(key)

    @classmethod
    def from_local_state(cls, local_state: Path) -> "AesGcm":
        import json
        state = json.loads(local_state.read_text(encoding="utf-8"))
        enc = b64decode(state["os_crypt"]["encrypted_key"])
        if enc[:5] != b"DPAPI":
            raise RuntimeError(f"{local_state}: неизвестный формат ключа")
        return cls(dpapi_unprotect(enc[5:]))

    def _run(self, fn, nonce: bytes, data: bytes, tag: bytes, aad: bytes = b""):
        n = ctypes.create_string_buffer(nonce, len(nonce))
        t = ctypes.create_string_buffer(tag, 16)
        info = _AuthInfo()
        info.cbSize, info.dwInfoVersion = ctypes.sizeof(info), 1
        info.pbNonce, info.cbNonce = ctypes.cast(n, ctypes.c_void_p), len(nonce)
        info.pbTag, info.cbTag = ctypes.cast(t, ctypes.c_void_p), 16
        a = None
        if aad:
            a = ctypes.create_string_buffer(aad, len(aad))
            info.pbAuthData, info.cbAuthData = ctypes.cast(a, ctypes.c_void_p), len(aad)
        src = ctypes.create_string_buffer(data, max(len(data), 1))
        dst = ctypes.create_string_buffer(max(len(data), 1))
        got = wintypes.ULONG()
        _ok(fn(self._key, src, len(data), ctypes.byref(info), None, 0, dst, len(data), ctypes.byref(got), 0))
        del a  # держим буфер живым до конца вызова
        return dst.raw[: got.value], t.raw

    def decrypt(self, blob: bytes, aad: bytes = b"") -> bytes:
        """Блоб формата v10 (как в os_crypt Chromium)."""
        if blob[:3] != b"v10":
            raise ValueError("не v10")
        body = blob[15:]
        return self._run(_bcrypt.BCryptDecrypt, blob[3:15], body[:-16], body[-16:], aad)[0]

    def decrypt_gcm(self, nonce: bytes, body_with_tag: bytes, aad: bytes = b"") -> bytes:
        """Сырой GCM: nonce + (шифртекст + тег). Без префикса v10."""
        return self._run(_bcrypt.BCryptDecrypt, nonce, body_with_tag[:-16], body_with_tag[-16:], aad)[0]

    def encrypt(self, plain: bytes) -> bytes:
        nonce = os.urandom(12)
        ct, tag = self._run(_bcrypt.BCryptEncrypt, nonce, plain, b"\0" * 16)
        return b"v10" + nonce + ct + tag


# ------------------------------------------------------------------ helpers


def running(exe: str) -> bool:
    import subprocess
    out = subprocess.run(["tasklist", "/FI", f"IMAGENAME eq {exe}", "/NH", "/FO", "CSV"],
                         capture_output=True, text=True, errors="replace").stdout
    return f'"{exe.lower()}"' in out.lower()


def chrome_time_to_unix(t) -> int:
    t = int(t or 0)
    return max(0, t // 1_000_000 - CHROME_EPOCH) if t else 0


def web_url(url: str) -> bool:
    return url.startswith(("http://", "https://"))


HOST_ALIASES = {"gmail.com": "mail.google.com"}


def url_key(url: str) -> str:
    """Для сравнения дублей: без схемы, www и завершающего '/'."""
    u = url.split("://", 1)[-1]
    host, _, rest = u.partition("/")
    host = host.lower()
    if host.startswith("www."):
        host = host[4:]
    host = HOST_ALIASES.get(host, host)
    return (host + "/" + rest).rstrip("/")


def read_json(path: Path, default):
    import json
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def write_json(path: Path, value):
    import json
    tmp = path.with_name(path.name + ".import.tmp")
    tmp.write_text(json.dumps(value, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
    os.replace(tmp, path)


class SourceDb:
    """Копия базы браузера во временной папке: читаем, не трогая оригинал."""

    def __init__(self, path: Path, tmp: str):
        dst = Path(tmp) / (path.name.replace(" ", "_") + f".{id(self)}")
        shutil.copy2(path, dst)
        self.db = sqlite3.connect(str(dst))
        self.db.row_factory = sqlite3.Row

    def __enter__(self):
        return self.db

    def __exit__(self, *exc):
        self.db.close()


def columns(db, table):
    return [r[1] for r in db.execute(f"pragma table_info({table})")]


def meta_version(db) -> int:
    row = db.execute("select value from meta where key='version'").fetchone()
    return int(row[0]) if row else 0


# ------------------------------------------------------------------ protobuf (Табло у Яндекса)


def _varint(b, p):
    r = s = 0
    while True:
        c = b[p]
        p += 1
        r |= (c & 0x7F) << s
        s += 7
        if c < 0x80:
            return r, p


def pb_fields(b):
    """(номер поля, wire type, значение) по protobuf-сообщению. Без схемы."""
    p = 0
    while p < len(b):
        key, p = _varint(b, p)
        f, wt = key >> 3, key & 7
        if wt == 0:
            v, p = _varint(b, p)
        elif wt == 2:
            n, p = _varint(b, p)
            v = b[p: p + n]
            p += n
        elif wt == 1:
            v = b[p: p + 8]
            p += 8
        elif wt == 5:
            v = b[p: p + 4]
            p += 4
        else:
            raise ValueError(f"wire type {wt}")
        yield f, wt, v


# ------------------------------------------------------------------ SNSS session (общая часть)


class Pickle:
    """Читатель base::Pickle из файла сессии (SNSS). Числа — little-endian,
    строки выровнены по 4 байта."""

    def __init__(self, b):
        self.b, self.p = b, 4  # первые 4 байта — размер

    def i32(self):
        import struct
        v = struct.unpack_from("<i", self.b, self.p)[0]
        self.p += 4
        return v

    def s8(self):
        n = self.i32()
        v = self.b[self.p: self.p + n]
        self.p += (n + 3) & ~3
        return v.decode("utf-8", "replace")

    def s16(self):
        n = self.i32()
        v = self.b[self.p: self.p + 2 * n]
        self.p += (2 * n + 3) & ~3
        return v.decode("utf-16-le", "replace")


def iter_snss_commands(path: Path):
    """(id команды, payload) по файлу сессии. Проверяет сигнатуру SNSS."""
    import struct
    data = path.read_bytes()
    if data[:4] != b"SNSS":
        raise ValueError("не SNSS")
    pos = 8
    while pos + 2 <= len(data):
        size = struct.unpack_from("<H", data, pos)[0]
        pos += 2
        if size == 0 or pos + size > len(data):
            break
        yield data[pos], data[pos + 1: pos + size]
        pos += size


# ------------------------------------------------------------------ passwords and cookies


def import_logins(src: Path, dst: Path, decrypt_source, dkey, tmp, dry, report,
                  label="Пароли", src_table="logins"):
    """Переносит пароли. decrypt_source(row) -> открытый пароль (bytes) или b"";
    исключение означает, что строку расшифровать не вышло."""
    with SourceDb(src, tmp) as s:
        rows = s.execute(f"select * from {src_table}").fetchall()
        scols = columns(s, src_table)
    if dry:
        report.append(f"{label}: {len(rows)}")
        return
    d = sqlite3.connect(str(dst))
    try:
        dcols = columns(d, "logins")
        common = [c for c in scols if c in dcols and c != "id"]
        edge_ext = d.execute("select 1 from sqlite_master where name='logins_edge_extended'").fetchone()
        sql = f"insert or ignore into logins ({','.join(common)}) values ({','.join('?' * len(common))})"
        added = dup = bad = 0
        for r in rows:
            try:
                plain = decrypt_source(r)
                pw = dkey.encrypt(plain) if plain else b""
            except (ValueError, OSError):
                bad += 1
                continue
            cur = d.execute(sql, [pw if c == "password_value" else r[c] for c in common])
            if cur.rowcount:
                added += 1
                if edge_ext:  # своя таблица Edge, одна строка на пароль
                    d.execute("insert or ignore into logins_edge_extended (id) values (?)", (cur.lastrowid,))
            else:
                dup += 1
        d.commit()
    finally:
        d.close()
    report.append(f"{label}: {added} новых" + (f", {dup} уже были" if dup else "") + (f", {bad} не расшифровались" if bad else ""))


def import_cookies(src: Path, dst: Path, skey, dkey, tmp, dry, report):
    now_chrome = int((time.time() + CHROME_EPOCH) * 1_000_000)
    with SourceDb(src, tmp) as s:
        rows = s.execute("select * from cookies").fetchall()
        scols, sver = columns(s, "cookies"), meta_version(s)
    live = [r for r in rows if not (r["has_expires"] and 0 < r["expires_utc"] < now_chrome)]
    if dry:
        report.append(f"Cookies: {len(live)} (просроченных пропущено: {len(rows) - len(live)})")
        return
    d = sqlite3.connect(str(dst))
    try:
        dcols, dver = columns(d, "cookies"), meta_version(d)
        common = [c for c in scols if c in dcols]
        sql = f"insert or replace into cookies ({','.join(common)}) values ({','.join('?' * len(common))})"
        added = bad = 0
        for r in live:
            ev = bytes(r["encrypted_value"] or b"")
            try:
                if ev:
                    plain = skey.decrypt(ev)
                    # С версии 24 перед значением лежит SHA-256 от host_key.
                    if sver >= 24 > dver:
                        plain = plain[32:]
                    elif dver >= 24 > sver:
                        plain = hashlib.sha256(r["host_key"].encode()).digest() + plain
                    ev = dkey.encrypt(plain)
            except (ValueError, OSError):
                bad += 1
                continue
            d.execute(sql, [ev if c == "encrypted_value" else r[c] for c in common])
            added += 1
        d.commit()
    finally:
        d.close()
    report.append(f"Cookies: {added}" + (f", {bad} не расшифровались" if bad else "")
                  + (f" (просроченных пропущено: {len(rows) - len(live)})" if len(rows) > len(live) else ""))


# ------------------------------------------------------------------ bookmarks, speed dial, history


def bookmark_tree(profile: Path):
    data = read_json(profile / "Bookmarks", {})
    return [node for name, node in data.get("roots", {}).items()
            if name != "trash" and isinstance(node, dict) and "type" in node]


def walk_urls(node):
    if node.get("type") == "url":
        yield node
    for ch in node.get("children", []):
        yield from walk_urls(ch)


def import_bookmarks(profile: Path, data_dir: Path, dry, report, skip=None):
    path = data_dir / "bookmarks.json"
    items = read_json(path, [])
    have = {b["url"] for b in items}
    added = 0
    for root in bookmark_tree(profile):
        for n in walk_urls(root):
            url = n.get("url", "")
            if not web_url(url) or url in have or (skip and skip(n)):
                continue
            have.add(url)
            items.append({"url": url, "title": n.get("name") or url, "added": chrome_time_to_unix(n.get("date_added"))})
            added += 1
    if not dry:
        write_json(path, items)
    report.append(f"Закладки: {added} новых (всего {len(items)})")


def import_speed_dial(data_dir: Path, tiles, dry, report):
    """tiles — последовательность (url, title) уже отобранных плиток."""
    path = data_dir / "tablo.json"
    items = read_json(path, None)
    if items is None:  # Табло ещё не создано: те же плитки по умолчанию, что в src/store.rs
        items = [{"url": u, "title": t} for u, t in [
            ("https://www.youtube.com/", "YouTube"), ("https://mail.google.com/", "Gmail"),
            ("https://vk.com/", "ВКонтакте"), ("https://mail.ru/", "Главная Mail")]]
    have = {url_key(i["url"]) for i in items}
    added = 0
    for url, title in tiles:
        if not web_url(url) or url_key(url) in have:
            continue
        have.add(url_key(url))
        items.append({"url": url, "title": title or url.split("://", 1)[-1].split("/")[0]})
        added += 1
    if not dry:
        write_json(path, items)
    report.append(f"Табло (Экспресс-панель): {added} новых плиток (всего {len(items)})")


def import_history(profile: Path, data_dir: Path, tmp, dry, report):
    path = data_dir / "history.json"
    entries = {e["url"]: e for e in read_json(path, [])}
    before = len(entries)
    with SourceDb(profile / "History", tmp) as s:
        rows = s.execute(
            "select url, title, visit_count, last_visit_time from urls "
            "where hidden = 0 and visit_count > 0 and last_visit_time > 0 "
            "order by last_visit_time desc limit ?", (HISTORY_KEEP * 2,)).fetchall()
    for r in rows:
        url = r["url"]
        if not web_url(url) or len(url) > 2048:
            continue
        last, visits = chrome_time_to_unix(r["last_visit_time"]), int(r["visit_count"])
        e = entries.get(url)
        if e is None:
            entries[url] = {"url": url, "title": r["title"] or "", "visits": visits, "last": last}
        else:
            e["visits"] = max(e["visits"], visits)  # max, а не сумма: повторный запуск не удваивает
            if last > e["last"]:
                e["last"] = last
                e["title"] = r["title"] or e["title"]
    merged = sorted(entries.values(), key=lambda e: e["last"])[-HISTORY_KEEP:]
    if not dry:
        write_json(path, merged)
    report.append(f"История: {len(merged) - before:+} записей (всего {len(merged)})")


# ------------------------------------------------------------------ session -> spaces (общий сбор)


def merge_session(data_dir: Path, source_spaces, dry, report):
    """source_spaces — пространства-источники по порядку, каждое:
        {"name", "icon", "tabs": [{"url","title","pinned","active_at","selected"}]}.
    Сливает их в session.json: ищет пространство по имени или создаёт, не плодит
    дубли вкладок, ставит активную вкладку и открывается там же, где был источник."""
    path = data_dir / "session.json"
    session = read_json(path, {})
    spaces = session.get("spaces") or []
    if not spaces:  # пустая или старая сессия с одним списком вкладок
        spaces = [{"name": "Основное", "color": SPACE_COLORS[0], "icon": "",
                   "tabs": session.get("tabs", []), "active": session.get("active", 0)}]
    session.pop("tabs", None)

    def space_for(name, icon):
        for sp in spaces:
            if sp.get("name", "").strip().lower() == name.strip().lower():
                return sp
        sp = {"name": name, "color": SPACE_COLORS[len(spaces) % len(SPACE_COLORS)], "icon": icon, "tabs": [], "active": 0}
        spaces.append(sp)
        return sp

    added, best = 0, {}
    current = None
    for src in source_spaces:
        sp = space_for(src.get("name") or "Пространство", src.get("icon", ""))
        for t in src["tabs"]:
            urls = [x["url"] for x in sp["tabs"]]
            if t["url"] in urls:
                pos = urls.index(t["url"])
            else:
                sp["tabs"].append({"url": t["url"], "title": t["title"], "pinned": t.get("pinned", False)})
                pos = len(sp["tabs"]) - 1
                added += 1
            key = id(sp)
            if t.get("active_at", 0) >= best.get(key, (-1, 0))[0]:
                best[key] = (t.get("active_at", 0), pos)
            if t.get("selected"):
                current = (sp, pos)
    for sp in spaces:
        if id(sp) in best:
            sp["active"] = best[id(sp)][1]
        # Закреплённые — в начало, как в полосе вкладок.
        order = sorted(range(len(sp["tabs"])), key=lambda i: not sp["tabs"][i].get("pinned"))
        if order != list(range(len(order))):
            act = sp["active"]
            sp["tabs"] = [sp["tabs"][i] for i in order]
            sp["active"] = order.index(act) if act < len(order) else 0
            if current and current[0] is sp:
                current = (sp, order.index(current[1]))
    if current:  # открыться там же, где была активная вкладка источника
        current[0]["active"] = current[1]
        session["space"] = next(i for i, sp in enumerate(spaces) if sp is current[0])
    session["spaces"] = spaces
    session.setdefault("active", 0)
    if not dry:
        write_json(path, session)
    per = ", ".join(f"{sp['icon'] + ' ' if sp.get('icon') else ''}{sp['name']}: {len(sp['tabs'])}" for sp in spaces)
    report.append(f"Вкладки: {added} новых по пространствам ({per})")


# ------------------------------------------------------------------ extensions


def ext_name(manifest: dict, folder: Path) -> str:
    name = manifest.get("name") or folder.parent.name
    if name.startswith("__MSG_") and name.endswith("__"):
        key = name[6:-2].lower()
        msgs = read_json(folder / "_locales" / manifest.get("default_locale", "en") / "messages.json", {})
        name = next((v.get("message") for k, v in msgs.items() if k.lower() == key), folder.parent.name)
    return name


def import_extensions(profile: Path, data_dir: Path, engine_profile, dry, report):
    settings = read_json(profile / "Secure Preferences", {}).get("extensions", {}).get("settings", {})
    settings.update({k: v for k, v in read_json(profile / "Preferences", {}).get("extensions", {}).get("settings", {}).items()
                     if k not in settings})
    ext_dir = data_dir / "extensions"
    have_keys = set()
    for m in ext_dir.glob("*/manifest.json"):
        have_keys.add(read_json(m, {}).get("key"))
    done, skipped = [], []
    for d in sorted((profile / "Extensions").glob("*")):
        versions = sorted(p for p in d.glob("*") if (p / "manifest.json").is_file())
        if not versions:
            continue
        src = versions[-1]
        manifest = read_json(src / "manifest.json", {})
        name = ext_name(manifest, src)
        st = settings.get(d.name)
        if not st or st.get("disable_reasons") or st.get("state") == 0:
            skipped.append(name)
            continue
        if manifest.get("key") and manifest["key"] in have_keys:
            continue
        safe = "".join(c for c in name if c.isalnum() or c in " -_.()").strip()[:60] or d.name
        dst = ext_dir / safe
        if dst.exists():
            dst = ext_dir / d.name
        if not dry:
            shutil.copytree(src, dst, ignore=shutil.ignore_patterns("_metadata"))
            # Данные расширения (скрипты Tampermonkey, настройки). ID совпадает благодаря "key".
            if engine_profile and manifest.get("key"):
                for sub in ("Local Extension Settings", "Sync Extension Settings"):
                    s, t = profile / sub / d.name, engine_profile / sub / d.name
                    if s.is_dir() and not t.exists():
                        shutil.copytree(s, t, ignore=shutil.ignore_patterns("LOCK"))
                for s in (profile / "IndexedDB").glob(f"chrome-extension_{d.name}_*"):
                    t = engine_profile / "IndexedDB" / s.name
                    if s.is_dir() and not t.exists():
                        shutil.copytree(s, t, ignore=shutil.ignore_patterns("LOCK"))
        done.append(name)
    line = "Расширения: " + (", ".join(done) if done else "новых нет")
    if skipped:
        line += f" (выключенные пропущены: {', '.join(skipped)})"
    report.append(line)


# ------------------------------------------------------------------ target paths, backup, orchestration


def resolve_targets():
    """Папки MBrowser: данные (настройки) и профиль движка (пароли, cookies)."""
    home = os.environ.get("MBROWSER_HOME")
    data_dir = Path(home) if home else Path(os.environ["APPDATA"]) / "MBrowser"
    engine = (Path(home) / "Engine") if home else Path(os.environ["LOCALAPPDATA"]) / "MBrowser" / "Engine"
    engine_root = engine / "EBWebView"
    engine_profile = engine_root / "Default" if (engine_root / "Default").is_dir() else None
    return data_dir, engine_root, engine_profile


def backup(data_dir: Path, engine_profile):
    dst = data_dir / time.strftime("backup-%Y%m%d-%H%M%S")
    dst.mkdir()
    for f in data_dir.glob("*.json"):
        shutil.copy2(f, dst / f.name)
    if engine_profile:
        for rel in ("Login Data", "Network/Cookies"):
            if (engine_profile / rel).is_file():
                shutil.copy2(engine_profile / rel, dst / Path(rel).name)
    return dst


def import_secrets(local_state: Path, logins_src, logins_decrypt, cookies_src: Path,
                   engine_root, engine_profile, tmp, dry, report, logins_label="Пароли",
                   logins_table="logins"):
    """Пароли и cookies. logins_decrypt(skey) возвращает функцию row -> открытый
    пароль; ей передаётся ключ источника skey, чтобы браузеры со своим слоем
    шифрования (Яндекс) могли достроить его — она может и бросить исключение, если
    ключ получить не удалось (например, включён мастер-пароль). Профиль движка
    должен уже существовать."""
    if not (engine_profile and (engine_root / "Local State").is_file()):
        report.append("Пароли и cookies пропущены: профиля движка ещё нет — запустите и закройте MBrowser.")
        return
    skey = AesGcm.from_local_state(local_state)
    dkey = AesGcm.from_local_state(engine_root / "Local State")

    login_dst = engine_profile / "Login Data"
    if logins_src.is_file() and login_dst.is_file():
        try:
            decrypt_source = logins_decrypt(skey)
            import_logins(logins_src, login_dst, decrypt_source, dkey, tmp, dry, report, logins_label, logins_table)
        except sqlite3.OperationalError as e:
            report.append(f"{logins_label}: ошибка базы ({e}) — MBrowser точно закрыт?")
        except (RuntimeError, OSError, ValueError) as e:
            report.append(f"{logins_label} пропущены: {e}")
    else:
        report.append(f"{logins_label}: нет {login_dst if not login_dst.is_file() else logins_src}")

    cookie_dst = engine_profile / "Network" / "Cookies"
    if cookies_src.is_file() and cookie_dst.is_file():
        try:
            import_cookies(cookies_src, cookie_dst, skey, dkey, tmp, dry, report)
        except sqlite3.OperationalError as e:
            report.append(f"Cookies: ошибка базы ({e}) — MBrowser точно закрыт?")
    else:
        report.append(f"Cookies: нет {cookie_dst if not cookie_dst.is_file() else cookies_src}")
