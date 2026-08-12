#!/usr/bin/env python3
"""下载 PP-OCRv6 中英多语言页面 OCR 模型包(det + cls + rec -> 可直接运行的 ONNX)。

模型来自 RapidAI/RapidOCR 官方托管(ModelScope,Apache-2.0):
  - PP-OCRv6_det_small.onnx        (检测,~10MB)
  - PP-OCRv6_rec_small.onnx        (识别,多语言 50 种,~20MB)
  - ch_PP-LCNet_x0_25_textline_ori_cls_mobile.onnx (180° 旋转分类,复用 v5)
  - ppocrv6_dict.txt               (识别字典)
  - libonnxruntime.dylib           (macOS arm64,从公式模型包复制,可选)

产物(写入 $CARGO_HOME/ueberneon-page-ocr/paddle-ocr-v6-ch/ 或
UEBERNEON_PAGE_OCR_CACHE_DIR):
  - manifest.json / det_model.onnx / rec_model.onnx / cls_model.onnx
  - rec_dict.txt / libonnxruntime.dylib

依赖:仅 Python 3.10+ 标准库,无需 paddle2onnx / onnxruntime。
运行:`python3 scripts/export_paddle_ocr_onnx.py`
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import urllib.request
from pathlib import Path

BASE = "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2"
SIZES = {
    "tiny": {
        "det": f"{BASE}/onnx/PP-OCRv6/det/PP-OCRv6_det_tiny.onnx",
        "rec": f"{BASE}/onnx/PP-OCRv6/rec/PP-OCRv6_rec_tiny.onnx",
        "dict": f"{BASE}/paddle/PP-OCRv6/rec/PP-OCRv6_rec_tiny/ppocrv6_tiny_dict.txt",
    },
    "small": {
        "det": f"{BASE}/onnx/PP-OCRv6/det/PP-OCRv6_det_small.onnx",
        "rec": f"{BASE}/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx",
        "dict": f"{BASE}/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt",
    },
    "medium": {
        "det": f"{BASE}/onnx/PP-OCRv6/det/PP-OCRv6_det_medium.onnx",
        "rec": f"{BASE}/onnx/PP-OCRv6/rec/PP-OCRv6_rec_medium.onnx",
        "dict": f"{BASE}/paddle/PP-OCRv6/rec/PP-OCRv6_rec_medium/ppocrv6_dict.txt",
    },
}
CLS_URL = (
    f"{BASE}/onnx/PP-OCRv5/cls/ch_PP-LCNet_x0_25_textline_ori_cls_mobile.onnx"
)


def out_dir() -> Path:
    cache = os.environ.get("UEBERNEON_PAGE_OCR_CACHE_DIR")
    if cache:
        return Path(cache)
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    return cargo_home / "ueberneon-page-ocr" / "paddle-ocr-v6-ch"


def download(url: str, dest: Path) -> None:
    if dest.exists() and dest.stat().st_size > 0:
        print(f"已存在,跳过下载:{dest.name}")
        return
    print(f"下载 {url} -> {dest.name}")
    req = urllib.request.Request(url, headers={"User-Agent": "ueberneon/1.0"})
    with urllib.request.urlopen(req, timeout=300) as resp, open(dest, "wb") as f:
        shutil.copyfileobj(resp, f)


def copy_onnxruntime(work: Path) -> Path | None:
    local = os.environ.get("UEBERNEON_ONNXRUNTIME_DYLIB")
    candidates = []
    if local:
        candidates.append(Path(local))
    candidates.extend(
        [
            Path.home() / ".cargo" / "ueberneon-formula" / "unimernet",
            Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
            / "ueberneon-formula"
            / "unimernet",
        ]
    )
    for src in candidates:
        if src.is_file():
            shutil.copyfile(src, work / "libonnxruntime.dylib")
            print(f"ONNX Runtime 来自:{src}")
            return work / "libonnxruntime.dylib"
    print("警告:未找到 libonnxruntime.dylib,请设置 UEBERNEON_ONNXRUNTIME_DYLIB")
    return None


def write_manifest(work: Path, size: str) -> Path:
    manifest = {
        "name": f"PP-OCRv6 多语言 ({size})",
        "format": "paddle-ocr-v6-onnx",
        "language": "multi",
        "det_model": "det_model.onnx",
        "rec_model": "rec_model.onnx",
        "rec_dict": "rec_dict.txt",
        "cls_model": "cls_model.onnx",
        "rec_input_size": [48, 320],
        "cls_input_size": [80, 160],
        "mean": [0.5, 0.5, 0.5],
        "std": [0.5, 0.5, 0.5],
        "max_side": 2000,
        "det_limit_side_len": 736,
        "det_limit_type": "min",
        "det_thresh": 0.3,
        "box_thresh": 0.5,
        "unclip_ratio": 1.6,
        "use_dilation": True,
        "use_space_char": True,
        "cls_thresh": 0.9,
    }
    manifest_path = work / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return manifest_path


def main() -> int:
    size = os.environ.get("UEBERNEON_PAGE_OCR_SIZE", "small")
    if size not in SIZES:
        print(f"UEBERNEON_PAGE_OCR_SIZE 必须是 {list(SIZES)} 之一,当前:{size}", file=sys.stderr)
        return 2

    dest = out_dir()
    dest.mkdir(parents=True, exist_ok=True)
    urls = SIZES[size]
    download(urls["det"], dest / "det_model.onnx")
    download(urls["rec"], dest / "rec_model.onnx")
    download(urls["dict"], dest / "rec_dict.txt")
    download(CLS_URL, dest / "cls_model.onnx")
    copy_onnxruntime(dest)
    write_manifest(dest, size)

    print(f"完成。模型包:{dest}")
    print(
        "在设置中把「页面 OCR 模型目录」指向该目录,或写入 "
        "~/.ueberneon/settings.json 的 page_ocr.model_dir。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
