# C 阶段：从 Java 快照 DB 合成「真实封禁 IP 重放」fixture（工具脚本）
#
# 输出默认落在 target/dualrun/（gitignore 区）：fixture 含真实 IP，不入库；
# 用法：python gen_replay_fixture.py [快照目录或快照路径] [输出路径]
import sqlite3, json, sys, pathlib

SNAP = sys.argv[1] if len(sys.argv) > 1 else r"target/live/snapshots"
OUT = sys.argv[2] if len(sys.argv) > 2 else r"target/dualrun/replay_real.json"
if not (pathlib.Path(SNAP) / "java-peerbanhelper-nt.db").is_file():
    # 传入的是快照根目录：取名字排序最新的快照（KeepSnapshots 滚动清理旧快照）
    snaps = sorted(pathlib.Path(SNAP).glob("r*/java-peerbanhelper-nt.db"))
    if not snaps:
        sys.exit(f"未找到快照：{SNAP}")
    SNAP = str(snaps[-1].parent)
print(f"快照: {SNAP}")

conn = sqlite3.connect(f"{SNAP}/java-peerbanhelper-nt.db")

# 1) Java 封禁过的 (ip, port) —— 重放目标是「Java 封的，Rust 看到同样输入也要封」
banned = conn.execute(
    "SELECT DISTINCT ip, port FROM history WHERE ip IS NOT NULL AND ip != ''"
).fetchall()
# 2) 每个 address 在 peer_records 的最新属性（client/peerid/上传下载/flags/geoip）
latest = {}
for row in conn.execute(
    """SELECT address, port, client_name, peer_id, uploaded, upload_speed,
              downloaded, download_speed, last_flags, peer_geoip, last_time_seen
       FROM peer_records ORDER BY last_time_seen ASC"""
):
    latest[row[0].lower()] = row
conn.close()

HASH = "CC33CC33CC33CC33CC33CC33CC33CC33CC33CC33"
peers = []
matched = 0
for ip, port in banned:
    rec = latest.get(str(ip).lower())
    if rec:
        matched += 1
        _, _, client, pid, up, upspeed, dl, dlspeed, flags, geo, _ = rec
    else:
        client = pid = None
        up = upspeed = dl = dlspeed = 0
        flags = None
    peer = {
        "ip": str(ip),
        "port": int(port or 6881),
        "progress": 0.5,
        "flags": flags or "",
        "dl_speed": int(dlspeed or 0),
        "downloaded": int(dl or 0),
        "up_speed": int(upspeed or 0),
        "uploaded": int(up or 0),
        "connection": "TCP",
    }
    if client:
        peer["client"] = client
    if pid:
        peer["peer_id_client"] = pid
    peers.append(peer)

fixture = {
    "version": "5.0.0",
    "buildinfo": {"libtorrent": "1.2.19.0", "qt": "6.5.0"},
    "preferences": {"enable_multi_connections_from_same_ip": True},
    "torrents": [
        {
            "hash": HASH,
            "name": "Replay of real banned peers (from Java snapshot)",
            "progress": 1.0,
            "total_size": 4876000000,
            "piece_size": 16384,
            "pieces_have": 297241,
            "dlspeed": 0,
            "upspeed": 0,
            "is_private": False,
        }
    ],
    "peers": {HASH: peers},
}
with open(OUT, "w", encoding="utf-8") as f:
    json.dump(fixture, f, ensure_ascii=False, indent=2)
print(f"history 去重 (ip,port): {len(banned)}；peer_records 属性命中: {matched}")
print(f"fixture 已生成: {OUT}（{len(peers)} 个 peer）")
