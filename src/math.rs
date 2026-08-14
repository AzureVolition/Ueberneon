// ── 轻量 LaTeX → MathML 渲染 ──
//
// 消息面板把 `$...$` / `$$...$$` 转成 MathML（WebView 原生支持），
// 覆盖常见的上下标、分式、根式、希腊字母与简单矩阵。

/// 在 HTML 中把 `$...$` / `$$...$$` 替换为 MathML（跳过 <code>/<pre>）。
pub fn render_math_in_html(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let mut in_code = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if c == '<' {
            if starts_with_ci(&chars, i, "<code") || starts_with_ci(&chars, i, "<pre") {
                in_code = true;
            } else if starts_with_ci(&chars, i, "</code") || starts_with_ci(&chars, i, "</pre") {
                in_code = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if !in_code && c == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '$' {
                if let Some(end) = find_closing(&chars, i + 2, "$$") {
                    let tex: String = chars[i + 2..end].iter().collect();
                    if !tex.trim().is_empty() {
                        out.push_str(&format!(
                            "<div class=\"math-block\">{}</div>",
                            latex_to_mathml(&tex)
                        ));
                        i = end + 2;
                        continue;
                    }
                }
            } else if let Some(end) = find_closing(&chars, i + 1, "$") {
                let tex: String = chars[i + 1..end].iter().collect();
                if !tex.trim().is_empty() {
                    out.push_str(&format!(
                        "<span class=\"math-inline\">{}</span>",
                        latex_to_mathml(&tex)
                    ));
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn starts_with_ci(chars: &[char], pos: usize, token: &str) -> bool {
    let token: Vec<char> = token.chars().collect();
    if pos + token.len() > chars.len() {
        return false;
    }
    chars[pos..pos + token.len()]
        .iter()
        .zip(token.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn find_closing(chars: &[char], mut pos: usize, token: &str) -> Option<usize> {
    let token: Vec<char> = token.chars().collect();
    while pos + token.len() <= chars.len() {
        if chars[pos..pos + token.len()] == token[..] {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

/// 把一段 LaTeX 转成 MathML。
pub fn latex_to_mathml(latex: &str) -> String {
    let chars: Vec<char> = latex.chars().collect();
    let mut i = 0usize;
    format!("<math xmlns=\"http://www.w3.org/1998/Math/MathML\">{}</math>", parse_sequence(&chars, &mut i, false))
}

fn parse_sequence(chars: &[char], i: &mut usize, in_group: bool) -> String {
    let mut out = String::new();
    while *i < chars.len() {
        let c = chars[*i];
        if in_group && c == '}' {
            break;
        }
        if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            *i += 1;
            continue;
        }
        let mut base = parse_atom(chars, i);
        // 上标/下标：a^2、a_{ij}、x_i^2
        loop {
            if *i < chars.len() && chars[*i] == '^' {
                *i += 1;
                let sup = parse_script(chars, i);
                base = format!("<msup>{base}{sup}</msup>");
            } else if *i < chars.len() && chars[*i] == '_' {
                *i += 1;
                let sub = parse_script(chars, i);
                base = format!("<msub>{base}{sub}</msub>");
            } else {
                break;
            }
        }
        out.push_str(&base);
    }
    out
}

fn parse_atom(chars: &[char], i: &mut usize) -> String {
    if *i >= chars.len() {
        return String::new();
    }
    let c = chars[*i];
    if c == '{' {
        *i += 1;
        let inner = parse_sequence(chars, i, true);
        if *i < chars.len() && chars[*i] == '}' {
            *i += 1;
        }
        return format!("<mrow>{inner}</mrow>");
    }
    if c == '\\' {
        return parse_command(chars, i);
    }
    *i += 1;
    math_char(c)
}

fn parse_script(chars: &[char], i: &mut usize) -> String {
    parse_atom(chars, i)
}

fn parse_group(chars: &[char], i: &mut usize) -> String {
    parse_atom(chars, i)
}

fn parse_command(chars: &[char], i: &mut usize) -> String {
    *i += 1; // 吃掉反斜杠
    if *i >= chars.len() {
        return String::new();
    }
    if chars[*i].is_alphabetic() {
        let start = *i;
        while *i < chars.len() && chars[*i].is_alphabetic() {
            *i += 1;
        }
        let name: String = chars[start..*i].iter().collect();
        match name.as_str() {
            "frac" => {
                let n = parse_group(chars, i);
                let d = parse_group(chars, i);
                format!("<mfrac>{n}{d}</mfrac>")
            }
            "sqrt" => {
                let root = if *i < chars.len() && chars[*i] == '[' {
                    *i += 1;
                    let start = *i;
                    while *i < chars.len() && chars[*i] != ']' {
                        *i += 1;
                    }
                    let n: String = chars[start..(*i).min(chars.len())].iter().collect();
                    if *i < chars.len() && chars[*i] == ']' {
                        *i += 1;
                    }
                    n
                } else {
                    String::new()
                };
                let body = parse_group(chars, i);
                if root.is_empty() {
                    format!("<msqrt>{body}</msqrt>")
                } else {
                    format!("<mroot>{body}{}</mroot>", latex_to_mathml(&root))
                }
            }
            "left" | "right" => {
                while *i < chars.len() && chars[*i].is_whitespace() {
                    *i += 1;
                }
                if *i < chars.len() {
                    let d = chars[*i];
                    *i += 1;
                    let delim = match d {
                        '(' => "(",
                        ')' => ")",
                        '[' => "[",
                        ']' => "]",
                        '{' => "{",
                        '}' => "}",
                        '|' => "|",
                        '.' => "",
                        _ => "",
                    };
                    format!("<mo>{delim}</mo>")
                } else {
                    String::new()
                }
            }
            "text" => {
                if *i < chars.len() && chars[*i] == '{' {
                    *i += 1;
                    let start = *i;
                    while *i < chars.len() && chars[*i] != '}' {
                        *i += 1;
                    }
                    let t: String = chars[start..(*i).min(chars.len())].iter().collect();
                    if *i < chars.len() && chars[*i] == '}' {
                        *i += 1;
                    }
                    format!("<mtext>{}</mtext>", escape_xml(&t))
                } else {
                    String::new()
                }
            }
            "begin" => parse_matrix_env(chars, i),
            "cdot" => "<mo>·</mo>".into(),
            "times" => "<mo>×</mo>".into(),
            "div" => "<mo>÷</mo>".into(),
            "pm" => "<mo>±</mo>".into(),
            "mp" => "<mo>∓</mo>".into(),
            "leq" | "le" => "<mo>≤</mo>".into(),
            "geq" | "ge" => "<mo>≥</mo>".into(),
            "neq" | "ne" => "<mo>≠</mo>".into(),
            "approx" => "<mo>≈</mo>".into(),
            "equiv" => "<mo>≡</mo>".into(),
            "in" => "<mo>∈</mo>".into(),
            "notin" => "<mo>∉</mo>".into(),
            "subset" => "<mo>⊂</mo>".into(),
            "subseteq" => "<mo>⊆</mo>".into(),
            "cup" => "<mo>∪</mo>".into(),
            "cap" => "<mo>∩</mo>".into(),
            "emptyset" => "<mo>∅</mo>".into(),
            "forall" => "<mo>∀</mo>".into(),
            "exists" => "<mo>∃</mo>".into(),
            "nabla" => "<mo>∇</mo>".into(),
            "infty" => "<mo>∞</mo>".into(),
            "partial" => "<mo>∂</mo>".into(),
            "sum" => "<mo>∑</mo>".into(),
            "prod" => "<mo>∏</mo>".into(),
            "int" => "<mo>∫</mo>".into(),
            "rightarrow" | "to" => "<mo>→</mo>".into(),
            "leftarrow" => "<mo>←</mo>".into(),
            "Rightarrow" => "<mo>⇒</mo>".into(),
            "Leftarrow" => "<mo>⇐</mo>".into(),
            "leftrightarrow" => "<mo>↔</mo>".into(),
            "Leftrightarrow" => "<mo>⇔</mo>".into(),
            "ldots" | "dots" => "<mo>…</mo>".into(),
            "cdots" => "<mo>⋯</mo>".into(),
            "mid" | "vert" => "<mo>|</mo>".into(),
            "alpha" => "<mi>α</mi>".into(),
            "beta" => "<mi>β</mi>".into(),
            "gamma" => "<mi>γ</mi>".into(),
            "delta" => "<mi>δ</mi>".into(),
            "epsilon" => "<mi>ε</mi>".into(),
            "varepsilon" => "<mi>ϵ</mi>".into(),
            "zeta" => "<mi>ζ</mi>".into(),
            "eta" => "<mi>η</mi>".into(),
            "theta" => "<mi>θ</mi>".into(),
            "vartheta" => "<mi>ϑ</mi>".into(),
            "iota" => "<mi>ι</mi>".into(),
            "kappa" => "<mi>κ</mi>".into(),
            "lambda" => "<mi>λ</mi>".into(),
            "mu" => "<mi>μ</mi>".into(),
            "nu" => "<mi>ν</mi>".into(),
            "xi" => "<mi>ξ</mi>".into(),
            "pi" => "<mi>π</mi>".into(),
            "rho" => "<mi>ρ</mi>".into(),
            "sigma" => "<mi>σ</mi>".into(),
            "tau" => "<mi>τ</mi>".into(),
            "upsilon" => "<mi>υ</mi>".into(),
            "phi" => "<mi>φ</mi>".into(),
            "varphi" => "<mi>ϕ</mi>".into(),
            "chi" => "<mi>χ</mi>".into(),
            "psi" => "<mi>ψ</mi>".into(),
            "omega" => "<mi>ω</mi>".into(),
            "Gamma" => "<mi>Γ</mi>".into(),
            "Delta" => "<mi>Δ</mi>".into(),
            "Theta" => "<mi>Θ</mi>".into(),
            "Lambda" => "<mi>Λ</mi>".into(),
            "Xi" => "<mi>Ξ</mi>".into(),
            "Pi" => "<mi>Π</mi>".into(),
            "Sigma" => "<mi>Σ</mi>".into(),
            "Phi" => "<mi>Φ</mi>".into(),
            "Psi" => "<mi>Ψ</mi>".into(),
            "Omega" => "<mi>Ω</mi>".into(),
            _ => format!("<mi>{}</mi>", escape_xml(&name)),
        }
    } else {
        let c = chars[*i];
        *i += 1;
        match c {
            '{' => "{".into(),
            '}' => "}".into(),
            '%' => "%".into(),
            '_' => "_".into(),
            '\\' => "\\".into(),
            ' ' => " ".into(),
            _ => math_char(c),
        }
    }
}

fn parse_matrix_env(chars: &[char], i: &mut usize) -> String {
    // 已吃掉 \begin，读取 {env}
    if *i >= chars.len() || chars[*i] != '{' {
        return String::new();
    }
    *i += 1;
    let start = *i;
    while *i < chars.len() && chars[*i] != '}' {
        *i += 1;
    }
    let env: String = chars[start..(*i).min(chars.len())].iter().collect();
    if *i < chars.len() && chars[*i] == '}' {
        *i += 1;
    }
    let end_token = format!("\\end{{{env}}}");
    let token: Vec<char> = end_token.chars().collect();
    let mut pos = *i;
    let mut end = None;
    while pos + token.len() <= chars.len() {
        if chars[pos..pos + token.len()] == token[..] {
            end = Some(pos);
            break;
        }
        pos += 1;
    }
    let Some(end) = end else {
        return String::new();
    };
    let content: String = chars[*i..end].iter().collect();
    *i = end + token.len();
    parse_matrix(&content, &env)
}

fn parse_matrix(content: &str, env: &str) -> String {
    let rows: Vec<Vec<String>> = content
        .split("\\\\")
        .map(|row| {
            row.split('&')
                .map(|cell| latex_to_mathml(cell.trim()))
                .collect()
        })
        .collect();
    let body = rows
        .iter()
        .map(|cells| {
            let tds = cells
                .iter()
                .map(|c| format!("<mtd>{c}</mtd>"))
                .collect::<Vec<_>>()
                .join("");
            format!("<mtr>{tds}</mtr>")
        })
        .collect::<Vec<_>>()
        .join("");
    let (left, right) = match env {
        "pmatrix" => ("(", ")"),
        "bmatrix" => ("[", "]"),
        "Bmatrix" => ("{", "}"),
        "cases" => ("{", ""),
        _ => ("", ""),
    };
    format!("<mrow><mo>{left}</mo><mtable>{body}</mtable><mo>{right}</mo></mrow>")
}

fn math_char(c: char) -> String {
    match c {
        'a'..='z' | 'A'..='Z' => format!("<mi>{c}</mi>"),
        '0'..='9' => format!("<mn>{c}</mn>"),
        '*' => "<mo>·</mo>".into(),
        '+' | '-' | '=' | '<' | '>' | '(' | ')' | '[' | ']' | ',' | '.' | ':' | ';' | '|' => {
            format!("<mo>{}</mo>", escape_xml(&c.to_string()))
        }
        _ => escape_xml(&c.to_string()),
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_math_renders() {
        let html = render_math_in_html("<p>$a * b = b * a$</p>");
        assert!(html.contains("math-inline"), "{html}");
        assert!(html.contains("<mo>·</mo>"), "{html}");
        assert!(html.contains("<mo>=</mo>"), "{html}");
    }

    #[test]
    fn block_math_renders() {
        let html = render_math_in_html("<p>$$\\frac{a}{b}$$</p>");
        assert!(html.contains("math-block"), "{html}");
        assert!(html.contains("<mfrac>"), "{html}");
    }

    #[test]
    fn skips_code_blocks() {
        let html = render_math_in_html("<pre><code>$a$</code></pre>");
        assert!(!html.contains("math-inline"), "{html}");
        assert!(html.contains("$a$"), "{html}");
    }

    #[test]
    fn scripts_and_greek() {
        let out = latex_to_mathml("x_i^2 + \\alpha");
        assert!(out.contains("<msub>"), "{out}");
        assert!(out.contains("<msup>"), "{out}");
        assert!(out.contains("α"), "{out}");
    }
}
