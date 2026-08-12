# Third-Party Notices

## PDFium

本应用在构建期把 PDFium(预编译动态库)嵌入可执行文件,用于 PDF 阅读与文本提取。
二进制来自 bblanchon/pdfium-binaries release `chromium/7961`
(https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7961),
对应平台:`pdfium-mac-arm64.tgz`。

### 打包许可(MIT,Copyright 2014-2025 Benoit Blanchon)

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN
AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

### 组件许可

PDFium 本体(Google,BSD-3-Clause)及其依赖组件(FreeType、libpng、zlib、libjpeg、
OpenJPEG、ICU、abseil 等)的完整许可文本位于
https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7961/pdfium-mac-arm64.tgz
归档内的 `licenses/` 目录。

## ONNX Runtime

公式识别在构建期把 ONNX Runtime(macOS arm64 动态库)嵌入可执行文件,
运行时解压到用户缓存目录。版本:1.28.0(MIT License)。
https://github.com/microsoft/onnxruntime

## PP-FormulaNet_plus-S

公式识别模型 PP-FormulaNet_plus-S(PaddlePaddle 团队)按需通过
`scripts/export_unimernet_onnx.py` 导出并嵌入,许可为 Apache-2.0。
https://github.com/PaddlePaddle/PaddleOCR

## PaddleOCR PP-OCRv6

页面 OCR 模型 PP-OCRv6(det + cls + rec,PaddlePaddle 团队,由 RapidAI/RapidOCR
托管为 ONNX)按需通过 `scripts/export_paddle_ocr_onnx.py` 下载,许可为 Apache-2.0。
https://github.com/PaddlePaddle/PaddleOCR

## ort(ONNX Runtime Rust 绑定)

`ort` crate(pykeio/ort,MIT License)用于在 Rust 中加载 ONNX Runtime。
https://github.com/pykeio/ort
