#!/usr/bin/env python3
"""生成 tests/fixtures/sample.pdf:单页、含文本 "Hello PDFium" 的最小 PDF。

用途:PDFium FFI 层测试的固定输入。文件很小,直接提交到仓库。
"""

from pathlib import Path


def build() -> bytes:
    objects: list[bytes] = []

    def add(body: str | bytes) -> int:
        index = len(objects) + 1
        body_bytes = body.encode("latin-1") if isinstance(body, str) else body
        objects.append(
            f"{index} 0 obj\n".encode("ascii") + body_bytes + b"\nendobj\n"
        )
        return index

    add("<< /Type /Catalog /Pages 2 0 R >>")
    add("<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
    add(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        "/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
    )
    content = b"BT\n/F1 24 Tf\n72 720 Td\n(Hello PDFium) Tj\nET"
    add(
        f"<< /Length {len(content)} >>\nstream\n".encode("ascii")
        + content
        + b"\nendstream"
    )
    add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for obj in objects:
        offsets.append(len(out))
        out += obj

    xref_offset = len(out)
    out += b"xref\n0 6\n0000000000 65535 f \n"
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode("ascii")
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
        f"startxref\n{xref_offset}\n%%EOF\n"
    ).encode("ascii")
    return bytes(out)


def main() -> None:
    dest = Path(__file__).parent / "sample.pdf"
    dest.write_bytes(build())
    print(f"wrote {dest} ({dest.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
