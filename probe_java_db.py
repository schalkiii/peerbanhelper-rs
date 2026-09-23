import sqlite3
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "target/live/java_probe.db"
c = sqlite3.connect(path)
tables = [r[0] for r in c.execute("select name from sqlite_master where type='table' order by name")]
print("tables:", tables)
cols = [r[1] for r in c.execute("PRAGMA table_info(history)")]
print("history cols:", cols)
print("history rows:", c.execute("select count(*) from history").fetchone()[0])
for row in c.execute("select id, ban_at, ip, port, module_name from history order by id desc limit 3"):
    print("sample:", row)
for t in ("metadata", "alert", "torrents"):
    if t in tables:
        print(f"{t} rows:", c.execute(f"select count(*) from {t}").fetchone()[0])
if "metadata" in tables:
    rows = c.execute(
        "select k, v from metadata where k like 'dualrun%' or k like 'schema%'"
    ).fetchall()
    print("metadata(dualrun/schema):", rows)
