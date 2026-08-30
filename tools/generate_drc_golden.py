"""Generate src/drc/testdata/pipeline_16k_mono.json from pinned tss_algo_pipeline."""

from __future__ import annotations

import json
import math
import subprocess
import sys
from pathlib import Path

SOURCE_REPO = "tss_algo_pipeline"
SOURCE_COMMIT = "c61c5d6b904fa56055b06320810893e851c6e938"
SOURCE_ROOT = Path(r"C:\Projects\GitProjects\tss_algo_pipeline")
SAMPLE_RATE = 16000
PREDELAY_S = 0.002
CHUNK_SIZES = [1, 15, 16, 17, 80, 161, 256, 3, 511, 988]
MODE = "independent"

REPO_ROOT = Path(__file__).resolve().parents[1]
OUT_PATH = REPO_ROOT / "src" / "drc" / "testdata" / "pipeline_16k_mono.json"


def _git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(SOURCE_ROOT), *args], text=True).strip()


def build_input(n: int = 2048) -> "object":
    import numpy as np

    t = np.arange(n, dtype=np.float32) / np.float32(SAMPLE_RATE)
    x = np.zeros(n, dtype=np.float32)
    two_pi = np.float32(2.0 * math.pi)
    x[0:200] = np.float32(1.0e-5)
    x[200:400] = np.float32(0.0178) * np.sin(two_pi * np.float32(440.0) * t[200:400])
    x[400:900] = np.sin(two_pi * np.float32(440.0) * t[400:900]).astype(np.float32)
    x[900:1400] = np.float32(0.0316) * np.sin(two_pi * np.float32(440.0) * t[900:1400])
    x[1400:] = np.float32(0.5) * np.sin(two_pi * np.float32(440.0) * t[1400:])
    return x


def main() -> int:
    if not SOURCE_ROOT.is_dir():
        print(f"missing source repo: {SOURCE_ROOT}", file=sys.stderr)
        return 1
    dirty = _git("status", "--porcelain")
    if dirty:
        print("source repo working tree is not clean", file=sys.stderr)
        print(dirty, file=sys.stderr)
        return 1
    head = _git("rev-parse", "HEAD")
    if head != SOURCE_COMMIT:
        print(f"source HEAD {head} != {SOURCE_COMMIT}", file=sys.stderr)
        return 1

    sys.path.insert(0, str(SOURCE_ROOT / "src"))
    from tss_algo_pipeline.drc import DrcConfig, TssDrc
    import numpy as np

    assert sum(CHUNK_SIZES) == 2048
    audio = build_input(2048)
    config = DrcConfig.default()
    config.predelay_s = PREDELAY_S
    drc = TssDrc(SAMPLE_RATE, 1, config)
    parts = []
    offset = 0
    for size in CHUNK_SIZES:
        frame = audio[offset : offset + size]
        parts.append(drc.process_frame_independent(frame))
        offset += size
    expected = np.concatenate(parts)
    assert offset == 2048
    assert expected.shape == (2048,)
    assert np.all(np.isfinite(audio)) and np.all(np.isfinite(expected))

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "source_repo": SOURCE_REPO,
        "source_commit": SOURCE_COMMIT,
        "sample_rate": SAMPLE_RATE,
        "predelay_s": PREDELAY_S,
        "mode": MODE,
        "chunk_sizes": CHUNK_SIZES,
        "input": [float(v) for v in audio],
        "expected": [float(v) for v in expected],
    }
    OUT_PATH.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
