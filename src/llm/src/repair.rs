use crate::provider::{Message, Role};

/// 修复中断/恢复场景下不完整的 tool-call 配对
/// DeepSeek 会拒收 assistant tool_calls 后面没有对应 tool result 的请求
pub fn sanitize_tool_pairing(msgs: &[Message]) -> Vec<Message> {
    let mut out = Vec::new();
    let mut i = 0;

    while i < msgs.len() {
        let m = &msgs[i];

        if m.role == Role::Assistant && !m.tool_calls.is_empty() {
            // 推进到后续 tool 消息的末尾
            let mut j = i + 1;
            while j < msgs.len() && msgs[j].role == Role::Tool {
                j += 1;
            }

            // 修复截断的 JSON args
            let mut repaired = m.clone();
            for tc in &mut repaired.tool_calls {
                if !tc.arguments.is_empty()
                    && serde_json::from_str::<serde_json::Value>(&tc.arguments).is_err()
                {
                    tc.arguments = close_truncated_json(&tc.arguments);
                }
            }
            out.push(repaired);

            // 配对 tool 结果，缺失的补占位
            for tc in &m.tool_calls {
                let found = msgs[i + 1..j]
                    .iter()
                    .find(|tm| tm.tool_call_id.as_deref() == Some(&tc.id));
                match found {
                    Some(tm) => out.push(tm.clone()),
                    None => out.push(Message {
                        role: Role::Tool,
                        tool_call_id: Some(tc.id.clone()),
                        content: Some("[no result: interrupted]".into()),
                        ..Default::default()
                    }),
                }
            }
            i = j;
            continue;
        }

        // 孤立的 tool 消息 — 跳过
        if m.role == Role::Tool {
            i += 1;
            continue;
        }

        out.push(m.clone());
        i += 1;
    }

    out
}
fn close_truncated_json(s: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut esc = false;

    for c in s.chars() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }

    let mut out = s.to_string();
    if esc {
        out.pop();
    }
    if in_str {
        out.push('"');
    }

    let trimmed = out.trim_end();
    if trimmed.ends_with(',') {
        out.truncate(trimmed.len() - 1);
    } else if trimmed.ends_with(':') {
        out.push_str("null");
    }

    while let Some(c) = stack.pop() {
        out.push(c);
    }

    if serde_json::from_str::<serde_json::Value>(&out).is_err() {
        "{}".to_string()
    } else {
        out
    }
}
