// encoding 模块检测文件编码并将非 UTF-8 内容转换为 UTF-8。
//
// 检测级联：BOM → 严格 UTF-8 → GB18030 → 有损 UTF-8，
// 使得含 CJK 的 Windows 文件
// 可正常编辑而不会静默损坏其字节。
//
// 使用 encoding_rs 处理 GB18030 的转换。

use encoding_rs::GB18030;

/// 标识检测到的文件编码类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 纯 UTF-8，无 BOM — 最常见的情况。
    UTF8,
    /// UTF-8 带 BOM 前缀（EF BB BF）。
    UTF8BOM,
    /// UTF-16 Little-Endian 带 BOM（FF FE）。
    UTF16LE,
    /// UTF-16 Big-Endian 带 BOM（FE FF）。
    UTF16BE,
    /// GB18030（GBK 的超集，中国国家标准字符集）。
    GB18030,
    /// 不是有效 UTF-8 也不是有效 GB18030 — 以替换字符进行有损 UTF-8 解码。
    LossyUTF8,
    /// UTF-16 Little-Endian 无 BOM — Windows 工具保存的源文件常见。
    /// 通过 NUL 字节模式启发式检测；回写时不添加 BOM 以保留原始字节。
    UTF16LENoBOM,
    /// UTF-16 Big-Endian 无 BOM。
    UTF16BENoBOM,
}

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// 检测给定原始字节的编码类型。相同的字节应随后通过 `decode` 转换为
/// UTF-8 字符串。
pub fn detect(data: &[u8]) -> (Kind, &[u8]) {
    // 1. BOM 检测
    if data.len() >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
        return (Kind::UTF8BOM, data);
    }
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        return (Kind::UTF16LE, data);
    }
    if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
        return (Kind::UTF16BE, data);
    }

    // 2. BOM-less UTF-16 必须在 utf8::valid 之前检查：其低字节加 0x00 高字节
    //    都是合法的 UTF-8 码元，因此朴素检查会将 UTF-16 源文件误判为 UTF-8，
    //    从而暴露嵌入的 NUL 字符。
    if let Some(k) = detect_utf16_no_bom(data) {
        return (k, data);
    }

    // 3. 检查是否有效 UTF-8
    if let Ok(s) = std::str::from_utf8(data) {
        // 排除全为 ASCII 控制的纯 NUL 文件
        if !s.contains('\0') {
            return (Kind::UTF8, data);
        }
    }

    // 4. 尝试 GB18030 — 它是 GBK 的严格超集，拒绝真正无效的字节序列，
    //    因此成功解码是一个可靠信号。
    let (_, _, had_errors) = GB18030.decode(data);
    if !had_errors {
        return (Kind::GB18030, data);
    }

    // 5. 回退到有损 UTF-8
    (Kind::LossyUTF8, data)
}

/// 仅在开头几个字节中检查 BOM 前缀。这是用于 peek 二进制拒绝的快速路径：
/// 有 BOM 前缀的文件（UTF-16、UTF-8 BOM）跳过 NUL 字节检查，因为 0x00 在
/// UTF-16 中是正常的。对无 BOM 的内容返回 `None`（调用者应在验证没有 NUL
/// 字节后回退到完整的 `detect`）。
pub fn detect_quick(peek: &[u8]) -> Option<Kind> {
    if peek.len() >= 3 && peek[0] == 0xEF && peek[1] == 0xBB && peek[2] == 0xBF {
        return Some(Kind::UTF8BOM);
    }
    if peek.len() >= 2 && peek[0] == 0xFF && peek[1] == 0xFE {
        return Some(Kind::UTF16LE);
    }
    if peek.len() >= 2 && peek[0] == 0xFE && peek[1] == 0xFF {
        return Some(Kind::UTF16BE);
    }
    None
}

/// 启发式检测无 BOM 的 UTF-16，基于 NUL 字节分布：ASCII 范围的文本每个码元
/// 编码为一个有效字节和一个 0x00，因此 NUL 聚类在奇数偏移（LE）或偶数偏移
/// （BE）上。它要求强烈的偏斜——一个奇偶性高度 NUL，另一个几乎没有——这样
/// 真正的二进制（两个奇偶性都有 NUL）和纯 UTF-8（没有 NUL）都会回退。
fn detect_utf16_no_bom(b: &[u8]) -> Option<Kind> {
    let n = b.len();
    if n < 16 {
        return None;
    }
    // 检查偶数长度窗口以使奇偶计数可比
    let n = n & !1;

    let mut even_nul = 0usize;
    let mut odd_nul = 0usize;

    for i in 0..n {
        if b[i] == 0 {
            if i % 2 == 0 {
                even_nul += 1;
            } else {
                odd_nul += 1;
            }
        }
    }

    // 将数据分成两半（LE 和 BE 各占一半）
    let half = n / 2;

    // LE: 奇数偏移的 NUL 应该很多（≥ 30%），偶数偏移很少（≤ 5%）
    // 使用整数运算避免浮点：odd_nul * 10 >= half * 3 相当于 odd_nul / half >= 0.3
    if odd_nul * 10 >= half * 3 && even_nul * 20 <= half {
        return Some(Kind::UTF16LENoBOM);
    }
    // BE: 偶数偏移的 NUL 应该很多（≥ 30%），奇数偏移很少（≤ 5%）
    if even_nul * 10 >= half * 3 && odd_nul * 20 <= half {
        return Some(Kind::UTF16BENoBOM);
    }

    None
}

/// 将数据从给定编码转换为 UTF-8 字符串。
pub fn decode(data: &[u8], enc: Kind) -> String {
    match enc {
        Kind::UTF8BOM => {
            // 去除 BOM 前缀
            let content = if data.len() >= 3 && data.starts_with(UTF8_BOM) {
                &data[3..]
            } else {
                data
            };
            // 已经是有效 UTF-8
            String::from_utf8_lossy(content).into_owned()
        }
        Kind::UTF16LE => {
            let content = if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
                &data[2..]
            } else {
                data
            };
            decode_utf16(content, false)
        }
        Kind::UTF16BE => {
            let content = if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
                &data[2..]
            } else {
                data
            };
            decode_utf16(content, true)
        }
        Kind::UTF16LENoBOM => decode_utf16(data, false),
        Kind::UTF16BENoBOM => decode_utf16(data, true),
        Kind::GB18030 => {
            let (decoded, _, _) = GB18030.decode(data);
            decoded.into_owned()
        }
        Kind::UTF8 => {
            // 从 UTF-8 字节转为字符串；如果无效则用替换字符
            String::from_utf8_lossy(data).into_owned()
        }
        Kind::LossyUTF8 => {
            // 有损 UTF-8：直接使用 from_utf8_lossy
            String::from_utf8_lossy(data).into_owned()
        }
    }
}

/// 将 UTF-16 字节解码为 UTF-8 字符串。
/// BOM 应在调用前已剥离。
fn decode_utf16(data: &[u8], big_endian: bool) -> String {
    let code_units: Vec<u16> = if big_endian {
        data.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    } else {
        data.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    };

    let runes = utf16_decode(&code_units);
    String::from_iter(runes)
}

/// 将 UTF-16 码元转换为 rune，处理代理对。
fn utf16_decode(u: &[u16]) -> Vec<char> {
    let mut out = Vec::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        if (0xD800..=0xDBFF).contains(&c) && i + 1 < u.len() {
            let c2 = u[i + 1];
            if (0xDC00..=0xDFFF).contains(&c2) {
                let code_point = ((c as u32 - 0xD800) << 10) | (c2 as u32 - 0xDC00) | 0x1_0000;
                // SAFETY: 根据 UTF-16 代理对规则，code_point 在 0x10000..=0x10FFFF 范围内
                out.push(unsafe { char::from_u32_unchecked(code_point) });
                i += 2;
                continue;
            }
        }
        // 非法代理值或单个码元 → 直接转为 char
        out.push(char::from_u32(c as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
        i += 1;
    }
    out
}

/// 将 UTF-8 文本编码回指定的文件编码。
pub fn encode(text: &str, enc: Kind) -> Vec<u8> {
    match enc {
        Kind::UTF8BOM => {
            let mut out = UTF8_BOM.to_vec();
            out.extend_from_slice(text.as_bytes());
            out
        }
        Kind::UTF16LE => encode_utf16(text, false, true),
        Kind::UTF16BE => encode_utf16(text, true, true),
        Kind::UTF16LENoBOM => encode_utf16(text, false, false),
        Kind::UTF16BENoBOM => encode_utf16(text, true, false),
        Kind::GB18030 => {
            let (encoded, _, _) = GB18030.encode(text);
            encoded.into_owned()
        }
        Kind::UTF8 | Kind::LossyUTF8 => text.as_bytes().to_vec(),
    }
}

/// 将 UTF-8 文本编码为 UTF-16 字节，可选 BOM。
fn encode_utf16(text: &str, big_endian: bool, with_bom: bool) -> Vec<u8> {
    let runes: Vec<char> = text.chars().collect();
    let code_units = utf16_encode(&runes);

    let mut buf = Vec::with_capacity(if with_bom {
        2 + code_units.len() * 2
    } else {
        code_units.len() * 2
    });

    if with_bom {
        if big_endian {
            buf.extend_from_slice(&[0xFE, 0xFF]);
        } else {
            buf.extend_from_slice(&[0xFF, 0xFE]);
        }
    }

    for &u in &code_units {
        if big_endian {
            buf.extend_from_slice(&u.to_be_bytes());
        } else {
            buf.extend_from_slice(&u.to_le_bytes());
        }
    }

    buf
}

/// 将 rune 编码为 UTF-16 码元，对增补平面字符生成代理对。
fn utf16_encode(runes: &[char]) -> Vec<u16> {
    let mut out = Vec::with_capacity(runes.len());
    for &r in runes {
        let cp = r as u32;
        if cp >= 0x1_0000 && cp <= 0x10_FFFF {
            let cp = cp - 0x1_0000;
            out.push(0xD800 | ((cp >> 10) as u16));
            out.push(0xDC00 | (cp as u16 & 0x3FF));
        } else {
            out.push(r as u16);
        }
    }
    out
}

/// 读取并解码文件。加载整个文件并进行编码检测，返回 UTF-8 字符串。
pub fn read_file_to_string(path: &std::path::Path) -> std::io::Result<String> {
    let data = std::fs::read(path)?;
    let (enc, _) = detect(&data);
    Ok(decode(&data, enc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_utf8_plain() {
        let data = b"hello world";
        let (kind, _) = detect(data);
        assert_eq!(kind, Kind::UTF8);
    }

    #[test]
    fn detect_utf8_bom() {
        let data = b"\xEF\xBB\xBFhello";
        let (kind, _) = detect(data);
        assert_eq!(kind, Kind::UTF8BOM);
    }

    #[test]
    fn detect_utf16_le_bom() {
        // "hello" in UTF-16 LE with BOM
        let mut data = vec![0xFF, 0xFE];
        data.extend_from_slice(b"h\0e\0l\0l\0o\0");
        let (kind, _) = detect(&data);
        assert_eq!(kind, Kind::UTF16LE);
    }

    #[test]
    fn detect_utf16_be_bom() {
        // "hello" in UTF-16 BE with BOM
        let mut data = vec![0xFE, 0xFF];
        data.extend_from_slice(b"\0h\0e\0l\0l\0o");
        let (kind, _) = detect(&data);
        assert_eq!(kind, Kind::UTF16BE);
    }

    #[test]
    fn detect_utf16_le_no_bom() {
        // "hello world\n" in UTF-16 LE without BOM (≥16 bytes for heuristic)
        let data = b"h\0e\0l\0l\0o\0 \0w\0o\0r\0l\0d\0\n\0";
        let (kind, _) = detect(data);
        assert_eq!(kind, Kind::UTF16LENoBOM);
    }

    #[test]
    fn detect_utf16_be_no_bom() {
        // "hello world\n" in UTF-16 BE without BOM (≥16 bytes for heuristic)
        let data = b"\0h\0e\0l\0l\0o\0 \0w\0o\0r\0l\0d\0\n";
        let (kind, _) = detect(data);
        assert_eq!(kind, Kind::UTF16BENoBOM);
    }

    #[test]
    fn detect_lossy_utf8() {
        // Invalid UTF-8 sequence (0xFF is never valid UTF-8)
        let data = b"hello\xFFworld";
        // Should not be GB18030 either
        let (kind, _) = detect(data);
        assert_eq!(kind, Kind::LossyUTF8);
    }

    #[test]
    fn decode_utf8_roundtrip() {
        let data = b"hello world";
        let s = decode(data, Kind::UTF8);
        assert_eq!(s, "hello world");
    }

    #[test]
    fn decode_utf8_bom() {
        let data = b"\xEF\xBB\xBFhello";
        let s = decode(data, Kind::UTF8BOM);
        assert_eq!(s, "hello");
    }

    #[test]
    fn decode_utf16_le() {
        let mut data = vec![0xFF, 0xFE];
        data.extend_from_slice(b"h\0e\0l\0l\0o\0");
        let s = decode(&data, Kind::UTF16LE);
        assert_eq!(s, "hello");
    }

    #[test]
    fn decode_utf16_be() {
        let mut data = vec![0xFE, 0xFF];
        data.extend_from_slice(b"\0h\0e\0l\0l\0o");
        let s = decode(&data, Kind::UTF16BE);
        assert_eq!(s, "hello");
    }

    #[test]
    fn decode_utf16_le_no_bom() {
        let data = b"h\0e\0l\0l\0o\0";
        let s = decode(data, Kind::UTF16LENoBOM);
        assert_eq!(s, "hello");
    }

    #[test]
    fn encode_utf8_roundtrip() {
        let s = "hello world";
        let bytes = encode(s, Kind::UTF8);
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn encode_utf8_bom() {
        let s = "hello";
        let bytes = encode(s, Kind::UTF8BOM);
        assert_eq!(bytes, b"\xEF\xBB\xBFhello");
    }

    #[test]
    fn encode_decode_utf16_le() {
        let s = "hello";
        let bytes = encode(s, Kind::UTF16LE);
        // Has BOM
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let decoded = decode(&bytes, Kind::UTF16LE);
        assert_eq!(decoded, s);
    }

    #[test]
    fn encode_decode_utf16_le_no_bom() {
        let s = "hello";
        let bytes = encode(s, Kind::UTF16LENoBOM);
        assert!(!bytes.starts_with(&[0xFF, 0xFE]));
        let decoded = decode(&bytes, Kind::UTF16LENoBOM);
        assert_eq!(decoded, s);
    }

    #[test]
    fn detect_quick_bom() {
        assert_eq!(detect_quick(b"\xEF\xBB\xBF"), Some(Kind::UTF8BOM));
        assert_eq!(detect_quick(b"\xFF\xFE"), Some(Kind::UTF16LE));
        assert_eq!(detect_quick(b"\xFE\xFF"), Some(Kind::UTF16BE));
        assert_eq!(detect_quick(b"hello"), None);
    }

    #[test]
    fn short_data_not_utf16_no_bom() {
        // Less than 16 bytes should not be detected as BOM-less UTF-16
        let data = b"h\0e\0l";
        assert!(detect_utf16_no_bom(data).is_none());
    }

    #[test]
    fn utf16_surrogate_pair() {
        // U+1F600 (😀) in UTF-16 = D83D DE00
        let le_bytes: Vec<u8> = vec![0x3D, 0xD8, 0x00, 0xDE]; // LE
        let result = decode_utf16(&le_bytes, false);
        assert_eq!(result, "😀");

        let be_bytes: Vec<u8> = vec![0xD8, 0x3D, 0xDE, 0x00]; // BE
        let result = decode_utf16(&be_bytes, true);
        assert_eq!(result, "😀");
    }

    #[test]
    fn encode_decode_gb18030() {
        // "中文" in GB18030
        let s = "中文";
        let bytes = encode(s, Kind::GB18030);
        assert!(!bytes.is_empty());
        let decoded = decode(&bytes, Kind::GB18030);
        assert_eq!(decoded, s);
    }

    #[test]
    fn read_file_to_string_utf8() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_encoding_utf8.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let s = read_file_to_string(&path).unwrap();
        assert_eq!(s, "hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_file_to_string_utf16_le() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_encoding_utf16le.txt");
        let mut data = vec![0xFF, 0xFE];
        data.extend_from_slice(b"h\0e\0l\0l\0o\0");
        std::fs::write(&path, &data).unwrap();
        let s = read_file_to_string(&path).unwrap();
        assert_eq!(s, "hello");
        let _ = std::fs::remove_file(&path);
    }
}
