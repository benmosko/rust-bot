import sqlite3
conn = sqlite3.connect("logs/strategy.db")
tables = [t[0] for t in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")]
print("Tables:", tables)
for r in conn.execute("SELECT * FROM rounds"):
    print(r)
conn.close()
