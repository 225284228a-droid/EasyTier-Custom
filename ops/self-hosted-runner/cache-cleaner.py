#!/usr/bin/env python3
import fcntl
import os
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(os.environ.get("EASYTIER_CACHE_ROOT", "/srv/easytier-runner/cache"))
MAX_BYTES = int(os.environ.get("EASYTIER_CACHE_MAX_BYTES", str(16 * 1024**3)))
MAX_AGE_SECONDS = int(os.environ.get("EASYTIER_CACHE_MAX_AGE_SECONDS", str(7 * 86400)))
ACTIVE_MARKER = Path(os.environ.get("EASYTIER_CACHE_ACTIVE_MARKER", str(ROOT / ".job-active")))
ACTIVE_MAX_AGE_SECONDS = int(os.environ.get("EASYTIER_CACHE_ACTIVE_MAX_AGE_SECONDS", str(6 * 3600)))


def job_active():
    try:
        age = time.time() - ACTIVE_MARKER.stat().st_mtime
    except FileNotFoundError:
        return False
    if age <= ACTIVE_MAX_AGE_SECONDS:
        return True
    try:
        ACTIVE_MARKER.unlink()
    except FileNotFoundError:
        pass
    print(f"removed stale job marker {ACTIVE_MARKER}", flush=True)
    return False


def cache_unit(relative):
    parts = relative.parts
    if parts[:3] == ("cargo", "registry", "src") and len(parts) > 4:
        return ROOT.joinpath(*parts[:5])
    if parts[:3] == ("cargo", "registry", "cache"):
        return ROOT / relative
    if parts[:3] == ("cargo", "git", "checkouts") and len(parts) > 4:
        return ROOT.joinpath(*parts[:5])
    if parts[:3] == ("cargo", "git", "db") and len(parts) > 3:
        return ROOT.joinpath(*parts[:4])
    if parts[:2] == ("rustup", "toolchains") and len(parts) > 2:
        return ROOT.joinpath(*parts[:3])
    if parts[:1] == ("tools",) and len(parts) > 2:
        return ROOT.joinpath(*parts[:3])
    return ROOT / relative


def scan():
    units = {}
    now = time.time()
    for current, directories, files in os.walk(ROOT, topdown=True, followlinks=False):
        for name in files:
            path = Path(current) / name
            try:
                stat = path.lstat()
                if stat.st_size <= 0:
                    continue
                unit = cache_unit(path.relative_to(ROOT))
                item = units.setdefault(unit, {"size": 0, "last_used": 0})
                item["size"] += stat.st_size
                item["last_used"] = max(item["last_used"], stat.st_atime)
            except (FileNotFoundError, PermissionError, OSError):
                continue
    old = now - MAX_AGE_SECONDS
    return [pair for pair in units.items() if pair[1]["last_used"] < old], units


def remove(unit):
    try:
        if unit.is_dir() and not unit.is_symlink():
            shutil.rmtree(unit)
        else:
            unit.unlink(missing_ok=True)
        print(f"removed cache unit {unit}", flush=True)
        return True
    except FileNotFoundError:
        return True
    except OSError as exc:
        print(f"could not remove {unit}: {exc}", file=sys.stderr)
        return False


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    with open(ROOT / ".cache.lock", "a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("another cache cleanup is running", flush=True)
            return
        if job_active():
            print("active runner job detected; skipping cache cleanup", flush=True)
            return
        old_units, all_units = scan()
        for unit, _ in old_units:
            remove(unit)
        _, all_units = scan()
        remaining = sum(item["size"] for item in all_units.values())
        ordered = sorted(all_units.items(), key=lambda pair: (pair[1]["last_used"], str(pair[0])))
        for unit, item in ordered:
            if remaining <= MAX_BYTES:
                break
            if remove(unit):
                remaining -= item["size"]
        print(f"cache cleanup complete: about {remaining / 1024**3:.2f} GiB remain", flush=True)


if __name__ == "__main__":
    main()
