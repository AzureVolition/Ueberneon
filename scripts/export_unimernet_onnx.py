#!/usr/bin/env python3
"""一次性导出 UniMERNet 风格公式识别资源(PP-FormulaNet_plus-S -> 可运行 ONNX)。

产物(写入 $CARGO_HOME/ueberneon-formula/unimernet/ 或
UEBERNEON_FORMULA_CACHE_DIR):
  - model.onnx           (修补后的 ONNX,内部自带自回归 Loop)
  - libonnxruntime.dylib (ONNX Runtime 1.28.0, macOS arm64)
  - tokenizer.json       (BPE token id -> 字符串,供 Rust 端解码)
  - manifest.json        (预处理参数与后端声明)

依赖:Python 3.10+, paddlepaddle==3.0.0 paddlex==3.0.3 paddle2onnx
      onnx onnxruntime pyyaml
运行:`python3 scripts/export_unimernet_onnx.py`
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

import onnx
import yaml
from onnx import helper, numpy_helper

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
    return cargo_home / "ueberneon-formula" / "unimernet"


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
            tar.extractall(dest)


def find_paddle_model(work: Path) -> Path:
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
    raise RuntimeError("无法获取 UniMERNet 风格推理模型,请检查网络或设置 UEBERNEON_FORMULA_MODEL_TAR")


def convert_to_onnx(model_dir: Path, save_file: Path) -> None:
    cmd = [
        "paddle2onnx",
        "--model_dir",
        str(model_dir),
        "--model_filename",
        "inference.json",
        "--params_filename",
        "inference.pdiparams",
        "--save_file",
        str(save_file),
        "--enable_auto_update_opset",
        "True",
        "--enable_dist_prim_all",
        "True",
        "--enable_onnx_checker",
        "False",
        "--optimize_tool",
        "None",
    ]
    print("转换 ONNX:", " ".join(cmd))
    subprocess.run(cmd, check=True)
    if not save_file.exists():
        raise RuntimeError("paddle2onnx 未生成 model.onnx")


def patch_unimernet_loop(graph: onnx.GraphProto) -> int:
    """把 paddle2onnx 导坏了的 `If.3`(cond ? 1 : 0) 替换为等价的 Cast(cond)。

    paddle2onnx 2.1 把 Paddle while 里的终止条件导成 shape 不一致的 If:
    then 分支算出 1.0,else 分支直通标量 0.0,ORT 报
    `shape {} vs computed {1}`。该 If 恒等于 `Cast(cond -> float)`。
    """
    patched = 0
    for i, node in enumerate(graph.node):
        if node.op_type == "If" and node.name == "If.3":
            cond = node.input[0]
            out = node.output[0]
            cast = helper.make_node(
                "Cast", inputs=[cond], outputs=[out], to=onnx.TensorProto.FLOAT,
                name="fix.cast.if3",
            )
            graph.node[i].CopyFrom(cast)
            patched += 1
        for attr in node.attribute:
            if attr.type == onnx.AttributeProto.GRAPH:
                patched += patch_unimernet_loop(attr.g)
    return patched


def extract_tokenizer(model_dir: Path, work: Path) -> tuple[Path, list[str]]:
    yml_path = next(model_dir.rglob("inference.yml"))
    cfg = yaml.safe_load(yml_path.read_text(encoding="utf-8"))
    tok_cfg = cfg["PostProcess"]["character_dict"]["fast_tokenizer_file"]
    vocab = tok_cfg["model"]["vocab"]
    added = tok_cfg.get("added_tokens", [])

    max_id = -1
    for tid in vocab.values():
        max_id = max(max_id, int(tid))
    for item in added:
        max_id = max(max_id, int(item["id"]))
    tokens: list[str | None] = [None] * (max_id + 1)
    for token, tid in vocab.items():
        tokens[int(tid)] = token
    for item in added:
        tokens[int(item["id"])] = item["content"]

    tokenizer_path = work / "tokenizer.json"
    tokenizer_path.write_text(
        json.dumps(tokens, ensure_ascii=False), encoding="utf-8"
    )

    special: list[str] = []
    for item in added:
        content = item["content"]
        if content not in special:
            special.append(content)
    for s in ("<s>", "</s>", "<pad>", "<unk>"):
        if s not in special:
            special.append(s)
    return tokenizer_path, special


def write_manifest(work: Path, special: list[str]) -> Path:
    manifest = {
        "format": "unimernet-onnx",
        "name": "UniMERNet (PP-FormulaNet_plus-S)",
        "input_size": [384, 384],
        "mean": [0.7931, 0.7931, 0.7931],
        "std": [0.1738, 0.1738, 0.1738],
        "output": "token_ids",
        "tokenizer_file": "tokenizer.json",
        "special_tokens": special,
    }
    manifest_path = work / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return manifest_path


def copy_onnxruntime(work: Path) -> Path:
    candidates = [
        Path(sys.prefix) / "lib/python3.11/site-packages/onnxruntime/capi",
        Path(sys.prefix) / "lib/python3.12/site-packages/onnxruntime/capi",
    ]
    for capi in candidates:
        for lib in capi.glob("libonnxruntime.*.dylib"):
            dest = work / "libonnxruntime.dylib"
            shutil.copyfile(lib, dest)
            print(f"ONNX Runtime 来自 Python 包:{lib}")
            return dest
    raise RuntimeError("未找到 libonnxruntime dylib,请先 pip install onnxruntime")


def main() -> int:
    dest = out_dir()
    dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        model_dir = find_paddle_model(work)
        raw_onnx = work / "model_raw.onnx"
        convert_to_onnx(model_dir, raw_onnx)

        model = onnx.load(raw_onnx)
        patched = patch_unimernet_loop(model.graph)
        if patched == 0:
            raise RuntimeError("未找到需要修补的 If.3 节点,模型结构可能已变化")
        print(f"修补 If.3 节点: {patched}")
        onnx.checker.check_model(model)
        onnx_path = work / "model.onnx"
        onnx.save(model, onnx_path)

        tokenizer_path, special = extract_tokenizer(model_dir, work)
        manifest_path = write_manifest(work, special)
        lib_path = copy_onnxruntime(work)

        for src in (onnx_path, lib_path, tokenizer_path, manifest_path):
            shutil.copyfile(src, dest / src.name)
            print(f"产物 -> {dest / src.name}")
    print(f"完成。在设置中选择模型目录:{dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
