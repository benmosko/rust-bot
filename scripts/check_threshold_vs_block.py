"""Cross-check sniper_decision_events: threshold vs later blocks."""
import re
import sqlite3
from pathlib import Path

db = Path(__file__).resolve().parent.parent / "logs" / "strategy.db"
conn = sqlite3.connect(str(db))
conn.row_factory = sqlite3.Row
c = conn.cursor()


def parse_detail(d):
    if not d:
        return None, None
    y = re.search(r"yes_ask=Some\(([\d.]+)\)", d or "")
    n = re.search(r"no_ask=Some\(([\d.]+)\)", d or "")
    yv = float(y.group(1)) if y else None
    nv = float(n.group(1)) if n else None
    if d and "yes_ask=None" in d:
        yv = None
    if d and "no_ask=None" in d:
        nv = None
    return yv, nv


print("=== A) no_side_above_threshold: snapshot or detail with either side >= 0.96? ===")
rows = c.execute(
    "SELECT id, coin, round_start, detail, best_ask_yes, best_ask_no FROM sniper_decision_events WHERE reason_code='no_side_above_threshold'"
).fetchall()
suspicious = []
for r in rows:
    yv, nv = parse_detail(r["detail"])
    by, bn = r["best_ask_yes"], r["best_ask_no"]
    if (yv is not None and yv >= 0.96) or (nv is not None and nv >= 0.96):
        suspicious.append(("detail_parse", r))
    if (by is not None and by >= 0.96) or (bn is not None and bn >= 0.96):
        suspicious.append(("snapshot_cols", r))
print("total no_side rows:", len(rows))
print("suspicious (would contradict gate logic):", len(suspicious))
for kind, r in suspicious[:8]:
    print(kind, dict(r))

print()
print("=== B) Later-stage blocks: how often did snapshot show either ask >= 0.96? ===")
for reason in [
    "high_ask_tight_cex",
    "momentum_reversal_block",
    "side_of_open_block",  # legacy DB rows
    "distance_from_open_block",  # legacy DB rows
]:
    row = c.execute(
        """
        SELECT COUNT(*) AS n,
               SUM(CASE WHEN best_ask_yes >= 0.96 OR best_ask_no >= 0.96 THEN 1 ELSE 0 END) AS hit96
        FROM sniper_decision_events WHERE reason_code = ?
        """,
        (reason,),
    ).fetchone()
    print(reason, "n=", row["n"], "with yes>=0.96 OR no>=0.96:", row["hit96"])

print()
print("=== C) BTC 5m round 1774170600: alternate no_side vs side_of_open (same ts) ===")
for r in c.execute(
    """
    SELECT ts_utc, reason_code, best_ask_yes, best_ask_no, substr(detail,1,75)
    FROM sniper_decision_events
    WHERE round_start=1774170600 AND period='5m' AND coin='btc'
    ORDER BY ts_utc DESC LIMIT 15
    """
):
    print(dict(r))

conn.close()
