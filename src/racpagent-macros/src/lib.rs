
use proc_macro::TokenStream;

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
        _ => return Err(syn::Error::new_spanned(tool_name, "ToolMetaImpl only supports structs")),
    }

    // 解析 #[tool(...)] 辅助属性
    let is_read_only = has_tool_flag(&input.attrs, "read_only");
    let schema_str = get_tool_schema(&input.attrs).unwrap_or_else(|| String::new());
    let name_str = tool_name.to_string();
    let desc_str = description.as_str();

    let inventory_schema = if schema_str.is_empty() { "" } else { &schema_str };

    let expanded = quote::quote! {
        impl ::llm::tool::ToolMeta for #tool_name {
            fn name(&self) -> &str {
                #name_str
            }

            fn description(&self) -> &str {
                #desc_str
            }

            fn schema_str_str(&self) -> &str {
                #inventory_schema
            }

            fn read_only(&self) -> bool {
                #is_read_only
            }
        }

        #[cfg(not(test))]
        ::inventory::submit! {
            crate::tools::InternalToolMeta {
                name: #name_str,
                description: #desc_str,
                read_only: #is_read_only,
                schema: #inventory_schema,
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
                let found = meta.parse_nested_meta(|m| {
                    if m.path.is_ident(key) { Ok(()) } else { Err(m.error("")) }
                }).is_ok();
                if found {
                    return true
                }
            }
        }
    }
    false
}

/// 从 `#[tool(schema = "...")]` 中提取 schema JSON 字符串。
/// 支持普通字符串和 raw string (`r#"..."#`)。
fn get_tool_schema(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.path().is_ident("tool") {
            if let Ok(meta) = attr.meta.require_list() {
                let mut result: Option<String> = None;
                let _ = meta.parse_nested_meta(|m| {
                    if m.path.is_ident("schema") {
                        let val: syn::LitStr = m.value()?.parse()?;
                        result = Some(val.value());
                    }
                    Ok(())
                });
                if result.is_some() { return result; }
            }
        }
    }
    None
}

/// 从 `#[doc = "..."]` 属性中提取第一行文档注释。
fn extract_doc(attrs: &[syn::Attribute]) -> String {
    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let syn::Expr::Lit(lit) = &nv.value {
                    if let syn::Lit::Str(s) = &lit.lit {
                        let text = s.value();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            return trimmed.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
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
    fn extract_doc_first_non_empty() {
        let attrs: Vec<syn::Attribute> = vec![
            parse_quote!(#[doc = ""]),
            parse_quote!(#[doc = "first real line"]),
            parse_quote!(#[doc = "second line"]),
        ];
        assert_eq!(extract_doc(&attrs), "first real line");
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

    // ── get_tool_schema ──

    #[test]
    fn get_schema_absent() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(read_only)])];
        assert_eq!(get_tool_schema(&attrs), None);
    }

    #[test]
    fn get_schema_present() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(schema = r#"{"type":"object"}"#)])];
        assert_eq!(get_tool_schema(&attrs), Some(r#"{"type":"object"}"#.into()));
    }

    #[test]
    fn get_schema_no_tool_attr() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[doc = "docs only"])];
        assert_eq!(get_tool_schema(&attrs), None);
    }

    #[test]
    fn get_schema_empty_string() {
        let attrs: Vec<syn::Attribute> = vec![parse_quote!(#[tool(schema = "")])];
        assert_eq!(get_tool_schema(&attrs), Some(String::new()));
    }
}
