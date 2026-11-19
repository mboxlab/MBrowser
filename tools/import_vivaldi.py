"""Перенос данных из Vivaldi в MBrowser.

Переносит: вкладки и рабочие пространства, пароли, cookies (сайты остаются
залогинены), закладки, Экспресс-панель (в Табло), историю, включённые расширения
вместе с их данными.

    python tools/import_vivaldi.py             # перенос
    python tools/import_vivaldi.py --dry-run   # только показать, что будет перенесено

Оба браузера должны быть закрыты, MBrowser хотя бы раз запущен (иначе нет профиля
движка, куда класть пароли и cookies). Перед записью всё, что меняется, копируется
в <папка настроек MBrowser>\\backup-<время>. Повторный запуск дубликатов не создаёт.

Общий с другими браузерами код — в browser_import.py. Здесь — то, что у Vivaldi своё:
формат файла сессии и Экспресс-панель внутри дерева закладок.
"""

import argparse
import glob
import json
import os
import struct
import sys
import tempfile
import time
from pathlib import Path

import browser_import as bi

# ------------------------------------------------------------------ session (SNSS) -> tabs and spaces

# Номера команд в файле сессии Vivaldi. До 20 совпадают с Chromium; Vivaldi вставил
# 21 (JSON-данные вкладки) и 22 (данные окна), поэтому дальше номера сдвинуты на 2.
CMD_TAB_WINDOW, CMD_TAB_INDEX, CMD_NAV, CMD_SEL_NAV = 0, 2, 6, 7
CMD_SEL_TAB, CMD_WIN_TYPE, CMD_PINNED, CMD_TAB_CLOSED, CMD_WIN_CLOSED = 8, 9, 12, 16, 17
CMD_TAB_EXT, CMD_LAST_ACTIVE, CMD_NAV_PRUNED = 21, 23, 26


def parse_session(path: Path):
    tabs, wins = {}, {}
    tab = lambda t: tabs.setdefault(t, {"navs": {}, "sel": -1})
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
                wins.setdefault(w, {})["sel"] = i
            elif cid == CMD_WIN_TYPE:
                w, ty = struct.unpack_from("<ii", pl)
                wins.setdefault(w, {})["type"] = ty
            elif cid == CMD_PINNED:
                t, v = struct.unpack_from("<ii", pl)
                tab(t)["pinned"] = bool(v & 0xFF)
            elif cid == CMD_TAB_CLOSED:
                tab(struct.unpack_from("<i", pl)[0])["closed"] = True
            elif cid == CMD_WIN_CLOSED:
                wins.setdefault(struct.unpack_from("<i", pl)[0], {})["closed"] = True
            elif cid == CMD_TAB_EXT:
                p = bi.Pickle(pl)
                t = p.i32()
                tab(t)["ext"] = p.s8()
            elif cid == CMD_LAST_ACTIVE:
                t, ts = struct.unpack_from("<i4xq", pl)  # int64 выровнен по 8 байтам
                tab(t)["active_at"] = ts
            elif cid == CMD_NAV_PRUNED:
                t, i, n = struct.unpack_from("<iii", pl)
                tb = tab(t)
                tb["navs"] = {(k - n if k >= i + n else k): v for k, v in tb["navs"].items() if not i <= k < i + n}
                if i <= tb["sel"] < i + n:
                    tb["sel"] = i - 1
                elif tb["sel"] >= i + n:
                    tb["sel"] -= n
        except struct.error:
            continue  # обрезанная команда в конце файла
    return tabs, wins


def read_vivaldi_tabs(profile: Path):
    """Открытые вкладки обычных окон по порядку: dict(url, title, pinned, ws, active_at, selected)."""
    files = sorted(glob.glob(str(profile / "Sessions" / "Session_*")), key=os.path.getmtime, reverse=True)
    for f in files:
        try:
            tabs, wins = parse_session(Path(f))
        except (OSError, ValueError):
            continue
        out = []
        for tid, t in tabs.items():
            w = wins.get(t.get("win"), {})
            if t.get("closed") or "win" not in t or w.get("closed") or w.get("type", 0) != 0 or not t["navs"]:
                continue
            url, title = t["navs"].get(t["sel"]) or t["navs"][max(t["navs"])]
            if not bi.web_url(url):
                continue  # стартовая страница Vivaldi, chrome://, расширения
            try:
                ext = json.loads(t.get("ext") or "{}")
            except ValueError:
                ext = {}
            if ext.get("panelId"):
                continue  # веб-панель, не вкладка
            out.append({
                "url": url, "title": title, "pinned": t.get("pinned", False),
                "ws": ext.get("workspaceId"), "active_at": t.get("active_at", 0),
                "order": (t["win"], t.get("index", 1 << 30)),
                "selected": w.get("sel") == t.get("index"),
            })
        if out:
            out.sort(key=lambda x: x["order"])
            return out
    return []


def vivaldi_spaces(profile: Path):
    """Вкладки по рабочим пространствам Vivaldi — в формате merge_session."""
    tabs = read_vivaldi_tabs(profile)
    prefs = bi.read_json(profile / "Preferences", {})
    workspaces = prefs.get("vivaldi", {}).get("workspaces", {}).get("list", [])
    order = [(None, "Основное", "")]  # пространство по умолчанию идёт первым
    for w in workspaces:  # в порядке Vivaldi, даже пустые
        order.append((w.get("id"), w.get("name") or "Пространство", w.get("emoji") or ""))
    by_ws = {wid: {"name": name, "icon": icon, "tabs": []} for wid, name, icon in order}
    for t in tabs:
        sp = by_ws.get(t["ws"], by_ws[None])
        sp["tabs"].append({"url": t["url"], "title": t["title"], "pinned": t["pinned"],
                           "active_at": t["active_at"], "selected": t["selected"]})
    return [by_ws[wid] for wid, _, _ in order]


# ------------------------------------------------------------------ bookmarks & speed dial (Vivaldi)


def partner_link(node) -> bool:
    """Рекламные плитки и закладки, которые Vivaldi ставит сам."""
    return "Partner" in node.get("meta_info", {}) or "vivaldi.com/bk/" in node.get("url", "")


def speed_dial_folders(node):
    if node.get("type") != "folder":
        return
    if node.get("meta_info", {}).get("Speeddial") == "true":
        yield node
        return
    for ch in node.get("children", []):
        yield from speed_dial_folders(ch)


def vivaldi_tiles(profile: Path):
    """Плитки Экспресс-панели из папок закладок, помеченных Speeddial."""
    for root in bi.bookmark_tree(profile):
        for folder in speed_dial_folders(root):
            for n in bi.walk_urls(folder):
                url = n.get("url", "")
                if bi.web_url(url) and not partner_link(n):
                    yield url, n.get("name") or ""


# ------------------------------------------------------------------ main


def main():
    ap = argparse.ArgumentParser(description="Перенос данных из Vivaldi в MBrowser")
    ap.add_argument("--profile", type=Path,
                    default=Path(os.environ.get("LOCALAPPDATA", "")) / "Vivaldi" / "User Data" / "Default",
                    help="папка профиля Vivaldi")
    ap.add_argument("--dry-run", action="store_true", help="ничего не записывать, только показать")
    args = ap.parse_args()
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    profile = args.profile
    if not (profile / "Preferences").is_file():
        sys.exit(f"Не найден профиль Vivaldi: {profile}")
    data_dir, engine_root, engine_profile = bi.resolve_targets()

    busy = [n for n, exe in (("Vivaldi", "vivaldi.exe"), ("MBrowser", "mbrowser.exe")) if bi.running(exe)]
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
            logins_src=profile / "Login Data",
            logins_decrypt=lambda skey: (lambda r: skey.decrypt(bytes(r["password_value"])) if r["password_value"] else b""),
            cookies_src=profile / "Network" / "Cookies",
            engine_root=engine_root, engine_profile=engine_profile,
            tmp=tmp, dry=args.dry_run, report=report)
        bi.merge_session(data_dir, vivaldi_spaces(profile), args.dry_run, report)
        bi.import_bookmarks(profile, data_dir, args.dry_run, report, skip=partner_link)
        bi.import_speed_dial(data_dir, vivaldi_tiles(profile), args.dry_run, report)
        bi.import_history(profile, data_dir, tmp, args.dry_run, report)
        bi.import_extensions(profile, data_dir, engine_profile, args.dry_run, report)

    print("Будет перенесено (dry run):" if args.dry_run else "Перенесено:")
    for line in report:
        print("  • " + line)


if __name__ == "__main__":
    main()
