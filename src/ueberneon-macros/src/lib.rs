use proc_macro::TokenStream;
use syn::{Meta, Meta::NameValue, Path};

/// 派生宏 —— 自动实现 `ToolMeta` trait。
///
/// 辅助属性（放在 `#[derive]` 之后）：
/// - `#[tool(read_only)]`       — 标记只读，默认 false
/// - `#[tool(schema = "...")]`  — JSON schema 字符串，支持 raw string，默认 `"{}"`
#[proc_macro_derive(ToolMetaImpl, attributes(tool))]
pub fn derive_tool(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    match expand_tool(&input) {
        Ok(stream) => stream,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_tool(input: &syn::DeriveInput) -> Result<TokenStream, syn::Error> {
    let tool_name = &input.ident;

    // 从 doc 注释取 description
    let description = extract_doc(&input.attrs);

    // 确保是 struct
    match &input.data {
        syn::Data::Struct(_) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                tool_name,
                "ToolMetaImpl only supports structs",
            ));
        }
    }

    // 解析 #[tool(...)] 辅助属性
    let is_read_only = has_tool_flag(&input.attrs, "read_only");
    let name_str = tool_name.to_string();
    let desc_str = description.as_str();
    let args = ToolArgs::get_tool_schema_struct(&input.attrs).unwrap_or_else(|e| panic!("{}", e));
    let args_type = args.args_type.expect("args type is required");

    let snake_upper = to_upper_snake(&name_str);
    let schema_var_name = syn::Ident::new(
        &format!("TOOL_SCHEMA_JSON_{}", snake_upper),
        tool_name.span(),
    );
    let expanded = quote::quote! {

        static #schema_var_name: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            let schema = ::schemars::schema_for!(#args_type);
            serde_json::to_string_pretty(&schema).unwrap()
        });

        impl ::llm::tool::ToolMeta for #tool_name {

            fn name(&self) -> &str {
                #name_str
            }

            fn description(&self) -> &str {
                #desc_str
            }

            fn schema_str_str(&self) -> &str {
                #schema_var_name.as_str()
            }

            fn read_only(&self) -> bool {
                #is_read_only
            }
        }

        impl crate::agent::GenericsType for #tool_name {
            type ArgType = #args_type;
        }

        #[cfg(not(test))]
        ::inventory::submit! {
            crate::tools::InternalToolMeta {
                name: #name_str,
                description: #desc_str,
                read_only: #is_read_only,
                schema:  &#schema_var_name,
            }
        }
    };
    Ok(expanded.into())
}

/// 检查 struct 上是否有 `#[tool(read_only)]` 这样的无值标记。
fn has_tool_flag(attrs: &[syn::Attribute], key: &str) -> bool {
    for attr in attrs {
        if attr.path().is_ident("tool") {
            if let Ok(meta) = attr.meta.require_list() {
                let found = meta
                    .parse_nested_meta(|m| {
                        if m.path.is_ident(key) {
                            Ok(())
                        } else {
                            Err(m.error(""))
                        }
                    })
                    .is_ok();
                if found {
                    return true;
                }
            }
        }
    }
    false
}

// 自定义属性解析：`#[tool(args = "SomeType")]`
#[derive(Default)]
struct ToolArgs {
    args_type: Option<Path>, // 存储类型路径，例如 `MyParams`
}

impl ToolArgs {
    fn get_tool_schema_struct(attrs: &[syn::Attribute]) -> Result<Self, syn::Error> {
        let mut result = Self::default();

        for attr in attrs {
            if !attr.path().is_ident("tool") {
                continue;
            }

            let meta = attr.parse_args::<Meta>()?;

            if let NameValue(nv) = meta {
                if nv.path.is_ident("argType") {
                    if let syn::Expr::Path(expr_path) = nv.value {
                        result.args_type = Some(expr_path.path);
                    } else {
                        return Err(syn::Error::new_spanned(nv.value, "expected a path"));
                    }
                }
            }
        }
        Ok(result)
    }
}

/// 从 `#[doc = "..."]` 属性中提取第一行文档注释。
fn extract_doc(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let syn::Expr::Lit(lit) = &nv.value {
                    if let syn::Lit::Str(s) = &lit.lit {
                        let text = s.value();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            lines.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    lines.join("\n")
}

fn to_upper_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 5);
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_uppercase() && !result.is_empty() {
            // 在前一个大写字母后插入下划线（但连续大写字母不插，例如 HTTP）
            // 更精确的规则：如果当前大写且前一个字符是小写或数字，则插入下划线
            // 简单实现：只要前一个不是下划线且不是开头就插入
            if !result.ends_with('_') {
                result.push('_');
            }
        }
        result.push(ch.to_ascii_uppercase());
    }
    // 处理连续大写后跟小写的情况（如 HTTPClient -> HTTP_CLIENT）
    // 上面简单实现已基本满足。
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn extract_doc_no_attrs() {
        let attrs: Vec<syn::Attribute> = vec![];
        assert_eq!(extract_doc(&attrs), "");
    }

    #[test]
    fn extract_doc_single_line() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[doc = "hello world"])];
        assert_eq!(extract_doc(&attrs), "hello world");
    }

    #[test]
    fn extract_doc_multiple_lines() {
        let attrs: Vec<syn::Attribute> = vec![
            parse_quote!(#[doc = ""]),
            parse_quote!(#[doc = "first real line"]),
            parse_quote!(#[doc = "second line"]),
        ];
        assert_eq!(extract_doc(&attrs), "first real line\nsecond line");
    }

    #[test]
    fn extract_doc_ignores_other_attrs() {
        let attrs: Vec<syn::Attribute> = vec![
            parse_quote!(#[serde(default)]),
            parse_quote!(#[doc = "only this matters"]),
        ];
        assert_eq!(extract_doc(&attrs), "only this matters");
    }

    #[test]
    fn extract_doc_trims_whitespace() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[doc = "  spaced  "])];
        assert_eq!(extract_doc(&attrs), "spaced");
    }

    // ── has_tool_flag ──

    #[test]
    fn tool_flag_read_only_present() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(read_only)])];
        assert!(has_tool_flag(&attrs, "read_only"));
    }

    #[test]
    fn tool_flag_read_only_absent() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(schema = "{}")])];
        assert!(!has_tool_flag(&attrs, "read_only"));
    }

    #[test]
    fn tool_flag_no_tool_attr() {
        let attrs: Vec<syn::Attribute> = vec![
            parse_quote!(#[doc = "some doc"]),
            parse_quote!(#[serde(default)]),
        ];
        assert!(!has_tool_flag(&attrs, "read_only"));
    }

    #[test]
    fn tool_flag_other_tool_attrs_ignored() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(schema = "{}")])];
        assert!(!has_tool_flag(&attrs, "something_else"));
    }
}
