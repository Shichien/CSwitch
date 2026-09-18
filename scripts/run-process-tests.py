"""Run isolated gateway process tests against the compiled app, never the real Codex home."""
import os
from pathlib import Path
import subprocess
root = Path(__file__).resolve().parent.parent
binary = Path(os.environ.get("CARGO_TARGET_DIR", root / "src-tauri" / "target")) / "debug" / ("cswitch.exe" if os.name == "nt" else "cswitch")
if not binary.is_file():
    raise SystemExit(f"Build the app first: missing {binary}")
env = dict(os.environ, CSWITCH_TEST_BINARY=str(binary.resolve()))
raise SystemExit(subprocess.call(["cargo", "test", "--manifest-path", str(root / "src-tauri/Cargo.toml"), "--", "--ignored", "--test-threads=1"], cwd=root, env=env))
