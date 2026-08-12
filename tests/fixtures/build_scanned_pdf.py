#!/usr/bin/env python3
"""把 sips 转换出的 BMP 打包成最小单页 PDF。

默认输出纯扫描页:内嵌 RGB 图像(FlateDecode),没有任何文本层,
PDFium 提取不到字符,用于扫描页 OCR 测试。
加 --searchable 输出可搜索版:同一张图 + 隐藏文字层(Helvetica + STSong-Light),
在普通阅读器里也能选中文字(不会触发 OCR)。

运行:
  python3 tests/fixtures/build_scanned_pdf.py <输入.bmp> <输出.pdf>
  python3 tests/fixtures/build_scanned_pdf.py --searchable <输入.bmp> <输出.pdf>
"""

from __future__ import annotations

import sys
import zlib
from pathlib import Path


def read_bmp(path: Path) -> tuple[int, int, bytes]:
    data = path.read_bytes()
    if data[:2] != b"BM":
        raise ValueError("not a BMP")
    data_offset = int.from_bytes(data[10:14], "little")
    width = int.from_bytes(data[18:22], "little", signed=True)
    height = int.from_bytes(data[22:26], "little", signed=True)
    bpp = int.from_bytes(data[28:30], "little")
    compression = int.from_bytes(data[30:34], "little")
    if bpp != 24 or compression != 0:
        raise ValueError(f"unsupported BMP: bpp={bpp} compression={compression}")
    top_down = height < 0
    height = abs(height)
    stride = (width * 3 + 3) // 4 * 4
    pixels = bytearray(width * height * 3)
    for y in range(height):
        src_row = y if top_down else height - 1 - y
        src = data_offset + src_row * stride
        for x in range(width):
            b, g, r = data[src + x * 3], data[src + x * 3 + 1], data[src + x * 3 + 2]
            off = (y * width + x) * 3
            pixels[off] = r
            pixels[off + 1] = g
            pixels[off + 2] = b
    return width, height, bytes(pixels)


def build_pdf(width: int, height: int, rgb: bytes, searchable: bool = False) -> bytes:
    stream = zlib.compress(rgb, level=9)
    objects: list[bytes] = []

    def add(body: bytes) -> int:
        index = len(objects) + 1
        objects.append(
            f"{index} 0 obj\n".encode("ascii") + body + b"\nendobj\n"
        )
        return index

    # 可搜索版会在页面之前插入 5 个字体对象(3..7),页面编号随之后移;
    # 目录 /Kids 必须指向实际页面对象编号。
    page_idx = 8 if searchable else 3
    content_idx = page_idx + 1
    image_idx = page_idx + 2

    add(b"<< /Type /Catalog /Pages 2 0 R >>")
    add(f"<< /Type /Pages /Kids [{page_idx} 0 R] /Count 1 >>".encode("ascii"))
    if searchable:
        # F1 = 标准 Helvetica(Type1);F2 = STSong-Light Type0 + ToUnicode。
        add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")  # 3
        add(
            b"<< /Type /FontDescriptor /FontName /STSong-Light /Flags 4 "
            b"/FontBBox [-25 -254 1000 880] /ItalicAngle 0 /Ascent 880 "
            b"/Descent -120 /CapHeight 880 /StemV 80 >>"
        )  # 4
        add(
            b"<< /Type /Font /Subtype /CIDFontType0 /BaseFont /STSong-Light "
            b"/CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 5 >> "
            b"/FontDescriptor 4 0 R >>"
        )  # 5
        tou_unicode = (
            b"/CIDInit /ProcSet findresource begin\n"
            b"12 dict begin\n"
            b"begincmap\n"
            b"/CMapName /Adobe-Identity-UCS def\n"
            b"/CMapType 2 def\n"
            b"1 begincodespacerange\n"
            b"<0000> <FFFF>\n"
            b"endcodespacerange\n"
            b"2 beginbfchar\n"
            b"<4F60> <4F60>\n"
            b"<597D> <597D>\n"
            b"endbfchar\n"
            b"endcmap\n"
            b"CMapName currentdict /CMap defineresource pop\n"
            b"end\n"
            b"end"
        )
        add(
            f"<< /Length {len(tou_unicode)} >>\nstream\n".encode("ascii")
            + tou_unicode
            + b"\nendstream"
        )  # 6
        add(
            b"<< /Type /Font /Subtype /Type0 /BaseFont /STSong-Light "
            b"/Encoding /UniGB-UCS2-H /DescendantFonts [5 0 R] /ToUnicode 6 0 R >>"
        )  # 7

    font_res = " /F1 3 0 R /F2 7 0 R" if searchable else ""
    content = f"q {width} 0 0 {height} 0 0 cm /Im1 Do Q"
    if searchable:
        # 隐藏文字层(渲染模式 3),与图像内容大致重叠:
        # 第一行 "Hello OCR 你好 123",第二行 "scanned fixture"。
        content += (
            " BT 3 Tr"
            " /F1 14 Tf 100 48 Td (Hello OCR ) Tj"
            " /F2 14 Tf <4F60597D> Tj"
            " /F1 14 Tf ( 123) Tj"
            " ET"
            " BT 3 Tr /F1 14 Tf 16 78 Td (scanned fixture) Tj ET"
        )
    page = (
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] "
        f"/Resources << /XObject << /Im1 {image_idx} 0 R >> "
        f"/Font <<{font_res} >> >> /Contents {content_idx} 0 R >>"
    ).encode("ascii")
    add(page)  # 8
    add(
        f"<< /Length {len(content)} >>\nstream\n".encode("ascii")
        + content.encode("ascii")
        + b"\nendstream"
    )  # 9
    add(
        f"<< /Type /XObject /Subtype /Image /Width {width} /Height {height} "
        f"/ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode "
        f"/Length {len(stream)} >>\nstream\n".encode("ascii")
        + stream
        + b"\nendstream"
    )  # 10

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for obj in objects:
        offsets.append(len(out))
        out += obj

    xref_offset = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode("ascii")
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode("ascii")
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
        f"startxref\n{xref_offset}\n%%EOF\n"
    ).encode("ascii")
    return bytes(out)


def main() -> None:
    searchable = "--searchable" in sys.argv
    args = [a for a in sys.argv[1:] if a != "--searchable"]
    if len(args) != 2:
        raise SystemExit(__doc__)
    src = Path(args[0])
    dest = Path(args[1])
    w, h, rgb = read_bmp(src)
    dest.write_bytes(build_pdf(w, h, rgb, searchable))
    kind = "searchable" if searchable else "scanned"
    print(f"wrote {dest} ({dest.stat().st_size} bytes, {w}x{h}, {kind})")


if __name__ == "__main__":
    main()
