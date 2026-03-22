"""One-shot: set bot profile photo via Bot API setMyProfilePhoto (InputProfilePhoto + attach)."""
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    env_path = root / ".env"
    if not env_path.is_file():
        print("Missing .env", file=sys.stderr)
        return 2
    token = None
    for line in env_path.read_text(encoding="utf-8", errors="replace").splitlines():
        m = re.match(r"^\s*TELEGRAM_BOT_TOKEN\s*=\s*(.*)$", line)
        if m:
            token = m.group(1).strip().strip('"').strip("'")
            break
    if not token:
        print("TELEGRAM_BOT_TOKEN not set in .env", file=sys.stderr)
        return 3

    jpg = root / "assets" / "telegram_bot_avatar.jpg"
    if not jpg.is_file():
        jpg = Path(os.environ.get("TEMP", ".")) / "telegram_bot_avatar.jpg"
    if not jpg.is_file():
        print(
            "JPEG not found: assets/telegram_bot_avatar.jpg "
            "(or %TEMP%\\telegram_bot_avatar.jpg)",
            file=sys.stderr,
        )
        return 4

    boundary = "----PolymarketBotBoundary7MA4YWxkTrZu0gW"
    photo_json = '{"type":"static","photo":"attach://avatar"}'
    file_bytes = jpg.read_bytes()

    parts: list[bytes] = []
    parts.append(f"--{boundary}\r\n".encode())
    parts.append(
        b'Content-Disposition: form-data; name="photo"\r\n\r\n'
        + photo_json.encode("utf-8")
        + b"\r\n"
    )
    parts.append(f"--{boundary}\r\n".encode())
    parts.append(
        b'Content-Disposition: form-data; name="avatar"; filename="telegram_bot_avatar.jpg"\r\n'
        b"Content-Type: image/jpeg\r\n\r\n"
    )
    parts.append(file_bytes)
    parts.append(f"\r\n--{boundary}--\r\n".encode())
    body = b"".join(parts)

    req = urllib.request.Request(
        f"https://api.telegram.org/bot{token}/setMyProfilePhoto",
        data=body,
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            print(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        print(e.read().decode("utf-8", errors="replace"), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
