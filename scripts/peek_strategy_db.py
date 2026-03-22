"""Quick inspect logs/strategy.db — run from repo root: python scripts/peek_strategy_db.py"""
import sqlite3
import sys
from pathlib import Path

db = Path(__file__).resolve().parent.parent / "logs" / "strategy.db"
if len(sys.argv) > 1:
    db = Path(sys.argv[1])

conn = sqlite3.connect(str(db))
conn.row_factory = sqlite3.Row
c = conn.cursor()


def q(sql, *a):
    return c.execute(sql, a).fetchall()


print("db:", db)
print()
print("=== tables ===")
for r in q("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"):
    print(" ", r[0])

print()
print("=== sniper_decision_events ===")
try:
    n = q("SELECT COUNT(*) FROM sniper_decision_events")[0][0]
    print("count:", n)
    for row in q(
        """SELECT id, session_id, ts_utc, coin, period, reason_code,
        substr(COALESCE(detail,''),1,100) AS detail_preview,
        best_ask_yes, best_ask_no, time_remaining_secs, filled_count
        FROM sniper_decision_events ORDER BY id DESC LIMIT 30"""
    ):
        print(dict(row))
except sqlite3.OperationalError as e:
    print("error:", e)

print()
print("=== round_price_ticks ===")
try:
    n = q("SELECT COUNT(*) FROM round_price_ticks")[0][0]
    print("total rows:", n)
    sid = q("SELECT session_id FROM round_price_ticks ORDER BY id DESC LIMIT 1")
    if sid:
        s = sid[0][0]
        print("latest session_id:", s)
        print(
            "ticks for that session:",
            q("SELECT COUNT(*) FROM round_price_ticks WHERE session_id=?", s)[0][0],
        )
except sqlite3.OperationalError as e:
    print("error:", e)

print()
print("=== rounds (fills) ===")
try:
    n = q("SELECT COUNT(*) FROM rounds")[0][0]
    print("count:", n)
    for row in q(
        """SELECT session_id, timestamp_utc, coin, period, side, round_start, entry_price, shares
        FROM rounds ORDER BY id DESC LIMIT 10"""
    ):
        print(dict(row))
except sqlite3.OperationalError as e:
    print("error:", e)

conn.close()
