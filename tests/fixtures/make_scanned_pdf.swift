// 用 AppKit + PDFKit 渲染一张"扫描样式"小图并导出为单页 PDF。
// 画布只有 240x100,页面没有文本层,专门用于扫描页 OCR 测试。
// 生成后会再用 sips + build_scanned_pdf.py 压缩成最小 PDF。
// 运行:swift tests/fixtures/make_scanned_pdf.swift <输出.pdf>

import AppKit
import PDFKit

let width: CGFloat = 240
let height: CGFloat = 100

let image = NSImage(size: NSSize(width: width, height: height))
image.lockFocus()

// 白色纸面
NSColor.white.setFill()
NSRect(x: 0, y: 0, width: width, height: height).fill()

// 浅灰"图注块",模拟扫描页里的插图区域
NSColor(calibratedWhite: 0.85, alpha: 1).setFill()
NSRect(x: 16, y: 16, width: 72, height: 52).fill()

// 正文文字(中英混合)
let font = NSFont(name: "PingFangSC-Regular", size: 14) ?? NSFont.systemFont(ofSize: 14)
let attrs: [NSAttributedString.Key: Any] = [
    .font: font,
    .foregroundColor: NSColor.black,
]
"Hello OCR 你好 123".draw(at: NSPoint(x: 100, y: 48), withAttributes: attrs)
"scanned fixture".draw(at: NSPoint(x: 16, y: 78), withAttributes: attrs)

image.unlockFocus()

let page = PDFPage(image: image)!
let document = PDFDocument()
document.insert(page, at: 0)

let outPath = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "sample-scanned.pdf"
try! document.dataRepresentation()!.write(to: URL(fileURLWithPath: outPath))
print("wrote \(outPath)")
