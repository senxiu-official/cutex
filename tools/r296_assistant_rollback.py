"""Read the database versions used by the r296 -> r295 rollback."""
import argparse
from contextlib import closing
import importlib.util
from pathlib import Path
import sqlite3
import sys


STORES = (
    ("personal-assistants/assistants-v1.sqlite3", 4, "个人助理", "个人助理、Director 收件箱、助理议程和日历将暂停使用"),
    ("schedules/schedules-v1.sqlite3", 2, "定时任务", "所有定时任务都会停止，包括普通所有者定时任务和助理周期模板"),
)

def snapshot_reader():
    root = Path(__file__).resolve().parent
    source = root / "shared/sqlite_snapshot.py"
    if not source.is_file():
        source = root.parent / "scripts/r266-compat/sqlite_snapshot.py"
    spec = importlib.util.spec_from_file_location("assistant_rollback_snapshot", source)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def stamp(path):
    try:
        stat = path.stat()
    except FileNotFoundError:
        return None
    return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)


def version(database):
    wal = Path(str(database) + "-wal")
    for _ in range(3):
        before = stamp(database), stamp(wal)
        if before[0] is None:
            if before[1] is not None:
                raise ValueError("SQLite 主文件缺失，仍存在 WAL")
            return None
        if before[1] is None or before[1][2] == 0:
            # A plain mode=ro connection to a closed WAL database can create -wal/-shm.
            with closing(sqlite3.connect(database.as_uri()+"?mode=ro&immutable=1",uri=True)) as db:
                result = db.execute("PRAGMA user_version").fetchone()[0]
            if before == (stamp(database), stamp(wal)):
                return result
            continue
        reader = snapshot_reader()
        if hasattr(reader, "read_database"):
            main, log = reader.read_database(database)
        else:
            main, log = reader.read_file(database), reader.read_file(wal)
            if before != (stamp(database), stamp(wal)):
                continue
        db, _ = reader.connect_snapshot(main, log)
        with closing(db):
            return db.execute("PRAGMA user_version").fetchone()[0]
    raise RuntimeError("读取期间数据库发生变化")


def warn(home, stream=None):
    stream = sys.stderr if stream is None else stream
    home = Path(home).resolve(strict=True)
    if not home.is_dir():
        raise ValueError("指定的 cutex 数据目录不存在")
    warnings = []
    for relative, minimum, name, impact in STORES:
        database = home / "runtime" / relative
        try:
            current = version(database)
        except Exception as exc:
            raise RuntimeError(f"无法核对{name}数据库版本，请检查 {database} 后重试。") from exc
        if current is not None and current >= minimum:
            message = f"回退提醒：{name}数据库为 v{current}，r295 无法读取。{impact}。"
            warnings.append(message)
            print(message, file=stream, flush=True)
    if warnings:
        print("恢复方法见套件中的 R296-ASSISTANT-RESTORE.zh-CN.md。", file=stream, flush=True)
    return warnings


def before_rollback(deploy):
    if deploy.plan.get("release") == "r296":
        warn(deploy.home)


def before_recover(deploy, operation):
    if operation.get("kind") == "rollback":
        before_rollback(deploy)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, required=True)
    warn(parser.parse_args().home)
