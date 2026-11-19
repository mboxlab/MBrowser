"""Перенос данных из Яндекс Браузера в MBrowser.

Переносит: вкладки и группы (как пространства), пароли, cookies (сайты остаются
залогинены), закладки, Табло (Экспресс-панель, без рекламных плиток Яндекса),
историю, включённые расширения вместе с их данными.

    python tools/import_yandex.py             # перенос
    python tools/import_yandex.py --dry-run   # только показать, что будет перенесено

Оба браузера должны быть закрыты, MBrowser хотя бы раз запущен (иначе нет профиля
движка, куда класть пароли и cookies). Перед записью всё, что меняется, копируется
в <папка настроек MBrowser>\\backup-<время>. Повторный запуск дубликатов не создаёт.

Общий с другими браузерами код — в browser_import.py. Здесь — то, что у Яндекса своё:
пароли (поверх os_crypt свой ключ в Ya Passman Data), группы вкладок и Табло (protobuf).
Если в Яндексе включён мастер-пароль, пароли не переносятся — об этом скажет отчёт.
"""

import argparse
import glob
import hashlib
import os
import struct
import sys
import tempfile
from pathlib import Path

import browser_import as bi

# ------------------------------------------------------------------ passwords (Ya Passman Data)

# Поля, по которым Яндекс считает AAD для GCM пароля (SHA-1 от значений через \0).
_AAD_FIELDS = ("origin_url", "username_element", "username_value", "password_element", "signon_realm")


def _login_aad(row) -> bytes:
    return hashlib.sha1(b"\0".join((row[c] or "").encode("utf-8") for c in _AAD_FIELDS)).digest()


def _collect_v10(blob: bytes, out: list):
    """Все вложенные блобы формата v10 из protobuf-сообщения."""
    try:
        for _, wt, v in bi.pb_fields(blob):
            if wt != 2:
                continue
            if v[:3] == b"v10":
                out.append(v)
            else:
                _collect_v10(v, out)
    except (ValueError, IndexError):
        pass  # не protobuf — просто не углубляемся


def _password_key(passman: Path, skey, tmp) -> "bi.AesGcm":
    """Ключ, которым Яндекс шифрует пароли: лежит в meta.local_encryptor_data,
    завёрнут в os_crypt (v10) и расшифровывается системным ключом skey. Проверяется
    на реальной строке — поэтому при мастер-пароле (ключ не отдаётся) бросит ошибку."""
    with bi.SourceDb(passman, tmp) as s:
        row = s.execute("select value from meta where key='local_encryptor_data'").fetchone()
        sample = s.execute("select * from logins where length(password_value) > 0 limit 1").fetchone()
    if not row or not row[0]:
        raise RuntimeError("в Ya Passman Data нет ключа шифрования")
    blobs = []
    _collect_v10(bytes(row[0]), blobs)
    for blob in blobs:
        try:
            plain = skey.decrypt(blob)  # через DPAPI профиля Яндекса
        except (ValueError, OSError):
            continue
        for key in (plain[4:36], plain[:32]):  # перед ключом бывает 4-байтная сигнатура
            if len(key) != 32:
                continue
            cand = bi.AesGcm.from_raw(key)
            if sample is None:
                return cand  # нечем проверить (пустая база) — берём как есть
            pv = bytes(sample["password_value"])
            try:
                cand.decrypt_gcm(pv[:12], pv[12:], _login_aad(sample))
                return cand
            except (ValueError, OSError):
                pass
    raise RuntimeError("не удалось получить ключ (возможно, включён мастер-пароль Яндекса)")


def yandex_login_decrypt(passman: Path, tmp):
    """Фабрика для import_secrets: по системному ключу skey строит расшифровщик строки."""
    def factory(skey):
        inner = _password_key(passman, skey, tmp)

        def decrypt(row):
            pv = bytes(row["password_value"] or b"")
            if not pv:
                return b""  # заблокированный сайт или вход через провайдера
            return inner.decrypt_gcm(pv[:12], pv[12:], _login_aad(row))

        return decrypt

    return factory


# ------------------------------------------------------------------ session -> tabs and groups

# Номера команд в файле сессии как в обычном Chromium, плюс свои команды групп Яндекса.
CMD_TAB_WINDOW, CMD_TAB_INDEX, CMD_NAV, CMD_SEL_NAV, CMD_SEL_TAB = 0, 2, 6, 7, 8
CMD_WIN_TYPE, CMD_PINNED, CMD_LAST_ACTIVE = 9, 12, 21
CMD_GROUP_TAB, CMD_GROUP_ORDER, CMD_GROUP_META = 105, 107, 113


def parse_session(path: Path):
    tabs, groups, win = {}, {}, {}
    tab = lambda t: tabs.setdefault(t, {"navs": {}, "sel": -1})
    grp = lambda g: groups.setdefault(g, {})
    for cid, pl in bi.iter_snss_commands(path):
        try:
            if cid == CMD_TAB_WINDOW:
                w, t = struct.unpack_from("<ii", pl)
                tab(t)["win"] = w
            elif cid == CMD_TAB_INDEX:
                t, i = struct.unpack_from("<ii", pl)
                tab(t)["index"] = i
            elif cid == CMD_NAV:
                p = bi.Pickle(pl)
                t, i = p.i32(), p.i32()
                url = p.s8()
                tab(t)["navs"][i] = (url, p.s16())
            elif cid == CMD_SEL_NAV:
                t, i = struct.unpack_from("<ii", pl)
                tab(t)["sel"] = i
            elif cid == CMD_SEL_TAB:
                w, i = struct.unpack_from("<ii", pl)
                win["sel"] = i
            elif cid == CMD_WIN_TYPE:
                w, ty = struct.unpack_from("<ii", pl)
                win.setdefault("type", ty)
            elif cid == CMD_PINNED:
                t, v = struct.unpack_from("<ii", pl)
                tab(t)["pinned"] = bool(v & 0xFF)  # пишется несколько раз, важно последнее
            elif cid == CMD_LAST_ACTIVE:
                t, ts = struct.unpack_from("<i4xq", pl)  # int64 выровнен по 8 байтам
                tab(t)["active_at"] = ts
            elif cid == CMD_GROUP_TAB:
                g, t = struct.unpack_from("<ii", pl)
                tab(t)["group"] = g
            elif cid == CMD_GROUP_ORDER:
                g, i = struct.unpack_from("<ii", pl)
                grp(g)["order"] = i
            elif cid == CMD_GROUP_META:
                p = bi.Pickle(pl)
                g = p.i32()
                name = p.s16()
                n = p.i32()
                kv = {p.s8(): p.s8() for _ in range(n)}
                grp(g).update(name=name, type=kv.get("type", "2"))
        except struct.error:
            continue  # обрезанная команда в конце файла
    return tabs, groups, win


def yandex_spaces(profile: Path):
    """Вкладки по группам Яндекса — в формате merge_session. Группа «по умолчанию»
    (type 1) становится пространством «Основное», пользовательские группы — своими."""
    files = sorted(glob.glob(str(profile / "Sessions" / "Session_*")), key=os.path.getmtime, reverse=True)
    for f in files:
        try:
            tabs, groups, win = parse_session(Path(f))
        except (OSError, ValueError):
            continue
        # Пространства в порядке групп; группа type 1 — это «Основное».
        spaces, by_gid, default = [], {}, None
        for gid, g in sorted(groups.items(), key=lambda kv: kv[1].get("order", 1 << 30)):
            is_default = g.get("type") == "1"
            sp = {"name": "Основное" if is_default else (g.get("name") or "Группа"), "icon": "", "tabs": []}
            by_gid[gid] = sp
            spaces.append(sp)
            if is_default and default is None:
                default = sp
        if default is None:  # групп по умолчанию нет — заводим «Основное» для бесхозных вкладок
            default = {"name": "Основное", "icon": "", "tabs": []}
            spaces.insert(0, default)

        placed = False
        for t in sorted(tabs.values(), key=lambda t: t.get("index", 1 << 30)):
            if "win" not in t or not t["navs"]:
                continue
            url, title = t["navs"].get(t["sel"]) or t["navs"][max(t["navs"])]
            if not bi.web_url(url):
                continue  # стартовая страница, browser://, расширения
            sp = by_gid.get(t.get("group"), default)
            sp["tabs"].append({
                "url": url, "title": title, "pinned": t.get("pinned", False),
                "active_at": t.get("active_at", 0), "selected": win.get("sel") == t.get("index"),
            })
            placed = True
        if placed:
            return [sp for sp in spaces if sp["tabs"] or sp is default]
    return []


# ------------------------------------------------------------------ Tablo (speed dial, protobuf)


def yandex_tiles(profile: Path):
    """Плитки Табло. Внутри плитки: поле 1 = url, поле 3 = название (часто пусто),
    поле 4 = вид (2 — рекламная плитка Яндекса, пропускаем). Закреплённые лежат в
    поле 2 (обёрнуты ещё в поле 1), плитки сетки — прямо в поле 3; поле 5 — это
    автоподсказки по истории, их не берём."""
    path = profile / "Tablo"
    if not path.is_file():
        return
    try:
        top = list(bi.pb_fields(path.read_bytes()))
    except (ValueError, IndexError):
        return
    for f, wt, v in top:
        if wt != 2 or f not in (2, 3):
            continue
        try:
            tile = {tf: tv for tf, _, tv in bi.pb_fields(v)}
            if f == 2:  # закреплённая плитка завёрнута: сама плитка в поле 1
                inner = tile.get(1)
                if not isinstance(inner, bytes):
                    continue
                tile = {tf: tv for tf, _, tv in bi.pb_fields(inner)}
        except (ValueError, IndexError):
            continue
        url = tile.get(1, b"")
        if not isinstance(url, bytes):
            continue
        url = url.decode("utf-8", "replace")
        if not bi.web_url(url) or tile.get(4, 0) == 2:
            continue
        title = tile.get(3, b"")
        yield url, title.decode("utf-8", "replace") if isinstance(title, bytes) else ""


# ------------------------------------------------------------------ main


def main():
    ap = argparse.ArgumentParser(description="Перенос данных из Яндекс Браузера в MBrowser")
    ap.add_argument("--profile", type=Path,
                    default=Path(os.environ.get("LOCALAPPDATA", "")) / "Yandex" / "YandexBrowser" / "User Data" / "Default",
                    help="папка профиля Яндекс Браузера")
    ap.add_argument("--dry-run", action="store_true", help="ничего не записывать, только показать")
    args = ap.parse_args()
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    profile = args.profile
    if not (profile / "Preferences").is_file():
        sys.exit(f"Не найден профиль Яндекс Браузера: {profile}")
    data_dir, engine_root, engine_profile = bi.resolve_targets()

    busy = [n for n, exe in (("Яндекс Браузер", "browser.exe"), ("MBrowser", "mbrowser.exe")) if bi.running(exe)]
    if busy and not args.dry_run:
        sys.exit(f"Закройте {' и '.join(busy)} и запустите снова.")

    data_dir.mkdir(parents=True, exist_ok=True)
    (data_dir / "extensions").mkdir(exist_ok=True)
    if not args.dry_run:
        print(f"Резервная копия: {bi.backup(data_dir, engine_profile)}")

    report = []
    with tempfile.TemporaryDirectory(prefix="mbrowser-import-") as tmp:
        bi.import_secrets(
            local_state=profile.parent / "Local State",
            logins_src=profile / "Ya Passman Data",
            logins_decrypt=yandex_login_decrypt(profile / "Ya Passman Data", tmp),
            cookies_src=profile / "Network" / "Cookies",
            engine_root=engine_root, engine_profile=engine_profile,
            tmp=tmp, dry=args.dry_run, report=report)
        bi.merge_session(data_dir, yandex_spaces(profile), args.dry_run, report)
        bi.import_bookmarks(profile, data_dir, args.dry_run, report)
        bi.import_speed_dial(data_dir, yandex_tiles(profile), args.dry_run, report)
        bi.import_history(profile, data_dir, tmp, args.dry_run, report)
        bi.import_extensions(profile, data_dir, engine_profile, args.dry_run, report)

    print("Будет перенесено (dry run):" if args.dry_run else "Перенесено:")
    for line in report:
        print("  • " + line)


if __name__ == "__main__":
    main()
