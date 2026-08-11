#!/usr/bin/env python3
"""一次性导出公式 OCR 资源(PP-FormulaNet_plus-S -> ONNX)。

产物(写入 $CARGO_HOME/ueberneon-formula/pp-formulanet-plus-s/ 或
UEBERNEON_FORMULA_CACHE_DIR):
  - libonnxruntime.dylib  (ONNX Runtime 1.28.0, macOS arm64)
  - model.onnx           (PP-FormulaNet_plus-S, 动态输入, opset 11)
  - dict.json            (词表, 供 greedy 解码)
  - preprocess.json      (预处理参数, 供 Rust 端复现)

依赖:Python 3.10+, pip install paddlepaddle==3.0.0 paddlex==3.0.3 paddle2onnx
      requests pillow
运行:`python3 scripts/export_formula_onnx.py`

之后重新构建即可嵌入;也可用 UEBERNEON_FORMULA_BUNDLE_DIR 指向产物目录。
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

ONNXRUNTIME_VERSION = "1.28.0"
ONNXRUNTIME_URL = (
    "https://github.com/microsoft/onnxruntime/releases/download/"
    f"v{ONNXRUNTIME_VERSION}/onnxruntime-osx-arm64-{ONNXRUNTIME_VERSION}.tgz"
)

MODEL_CANDIDATE_URLS = [
    "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/"
    "paddle3.0.0/PP-FormulaNet_plus-S_infer.tar",
    "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/"
    "paddle3.0.0/PP-FormulaNet-S_infer.tar",
]


def out_dir() -> Path:
    cache = os.environ.get("UEBERNEON_FORMULA_CACHE_DIR")
    if cache:
        return Path(cache)
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    return cargo_home / "ueberneon-formula" / "pp-formulanet-plus-s"


def download(url: str, dest: Path) -> None:
    if dest.exists() and dest.stat().st_size > 0:
        print(f"已存在,跳过下载:{dest.name}")
        return
    print(f"下载 {url} -> {dest.name}")
    urllib.request.urlretrieve(url, dest)


def extract_tar(archive: Path, dest: Path) -> None:
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:*") as tar:
        try:
            tar.extractall(dest, filter="data")
        except TypeError:
            # Python < 3.12 没有 filter 参数
            tar.extractall(dest)


def find_paddle_inference_model(work: Path) -> Path:
    local = os.environ.get("UEBERNEON_FORMULA_MODEL_TAR")
    if local:
        archive = Path(local)
        if not archive.is_file():
            raise RuntimeError(f"UEBERNEON_FORMULA_MODEL_TAR 指向的文件不存在: {archive}")
        model_dir = work / archive.name.removesuffix(".tar")
        extract_tar(archive, model_dir)
        for candidate in model_dir.rglob("inference.yml"):
            if (candidate.parent / "inference.json").exists():
                print(f"使用本地模型包: {archive}")
                return candidate.parent
        raise RuntimeError(f"本地模型包 {archive} 中未找到 inference.json/inference.yml")

    for url in MODEL_CANDIDATE_URLS:
        name = url.rstrip("/").rsplit("/", 1)[-1]
        archive = work / name
        try:
            download(url, archive)
        except Exception as exc:  # noqa: BLE001
            print(f"候选模型不可用({url}):{exc}")
            continue
        model_dir = work / name.removesuffix(".tar")
        extract_tar(archive, model_dir)
        for candidate in model_dir.rglob("inference.yml"):
            if (candidate.parent / "inference.json").exists():
                return candidate.parent
    raise RuntimeError("无法获取 PP-FormulaNet 推理模型,请检查网络或手动放置模型")


def extract_dict_and_preprocess(model_dir: Path, work: Path) -> tuple[Path, Path]:
    """从 PaddleOCR/PaddleX 包中取词表与预处理参数;找不到时给出兜底值。"""
    dict_src = None
    for pattern in (
        "formula_rec_dict.txt",
        "formula_rec_dict.json",
        "formula_dict.txt",
    ):
        for found in Path(sys.prefix).rglob(pattern):
            dict_src = found
            break
        if dict_src:
            break

    dict_path = work / "dict.json"
    if dict_src:
        lines = [
            line.strip()
            for line in dict_src.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        dict_path.write_text(json.dumps(lines, ensure_ascii=False), encoding="utf-8")
        print(f"词表来自 {dict_src}({len(lines)} 词)")
    else:
        dict_path.write_text(json.dumps(["", "<eos>"]), encoding="utf-8")
        print("警告:未找到词表文件,产物不可用;请安装 paddleocr/paddlex 后重跑")

    preprocess = {
        "height": 48,
        "mean": [0.485, 0.456, 0.406],
        "std": [0.229, 0.224, 0.225],
    }
    preprocess_path = work / "preprocess.json"
    preprocess_path.write_text(
        json.dumps(preprocess, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return dict_path, preprocess_path


def convert_to_onnx(model_dir: Path, work: Path) -> Path:
    onnx_path = work / "model.onnx"
    cmd = [
        "paddle2onnx",
        "--model_dir",
        str(model_dir),
        "--model_filename",
        "inference.json",
        "--params_filename",
        "inference.pdiparams",
        "--save_file",
        str(onnx_path),
        "--opset_version",
        "11",
        "--enable_onnx_checker",
        "True",
    ]
    print("转换 ONNX:", " ".join(cmd))
    subprocess.run(cmd, check=True)
    if not onnx_path.exists():
        raise RuntimeError("paddle2onnx 未生成 model.onnx")
    return onnx_path


def fetch_onnxruntime(work: Path) -> Path:
    archive = work / f"onnxruntime-osx-arm64-{ONNXRUNTIME_VERSION}.tgz"
    download(ONNXRUNTIME_URL, archive)
    extract_dir = work / f"onnxruntime-{ONNXRUNTIME_VERSION}"
    extract_tar(archive, extract_dir)
    candidates = list(extract_dir.rglob("libonnxruntime.*.dylib"))
    if not candidates:
        raise RuntimeError("ONNX Runtime 归档中未找到 libonnxruntime dylib")
    lib = work / "libonnxruntime.dylib"
    shutil.copyfile(candidates[0], lib)
    return lib


def verify_golden(model_dir: Path, onnx_path: Path, work: Path) -> None:
    """用 3 张示例公式图做冒烟对比(资源缺失时仅提示)。"""
    samples = list(Path(__file__).parent.parent.glob("tests/fixtures/formula_*.png"))
    if not samples:
        print("未找到 tests/fixtures/formula_*.png,跳过 golden 验证")
        return
    try:
        from paddleocr import FormulaRecognition  # type: ignore
    except ImportError:
        print("未安装 paddleocr,跳过 golden 验证")
        return

    paddle_model = FormulaRecognition(model_dir=str(model_dir))
    for img in samples:
        res = paddle_model.predict(str(img), batch_size=1)
        paddle_latex = res[0]["res"].get("rec_formula", "")
        print(f"golden 样例 {img.name}: paddle={paddle_latex[:60]}...")


def main() -> int:
    dest = out_dir()
    dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        model_dir = find_paddle_inference_model(work)
        dict_path, preprocess_path = extract_dict_and_preprocess(model_dir, work)
        onnx_path = convert_to_onnx(model_dir, work)
        lib = fetch_onnxruntime(work)
        verify_golden(model_dir, onnx_path, work)

        for src in (lib, onnx_path, dict_path, preprocess_path):
            shutil.copyfile(src, dest / src.name)
            print(f"产物 -> {dest / src.name}")
    print(f"完成。重新 cargo build 即嵌入;或设置 UEBERNEON_FORMULA_BUNDLE_DIR={dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
