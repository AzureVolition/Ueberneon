
use proc_macro::TokenStream;
use syn::Fields;

/// 派生宏 —— 从 struct 的 `schema` 和 `read_only` 字段自动实现 Tool trait。
///
/// 标记了 `#[derive(ToolMetaImpl)]` 的结构体必须包含：
/// - `schema: serde_json::Value`  —— 工具参数的 JSON Schema
/// - `read_only: bool`             —— 是否只读
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

    // 提取命名子段
    let fields = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    tool_name,
                    "ToolMetaImpl only supports structs with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                tool_name,
                "ToolMetaImpl only supports structs",
            ))
        }
    };

    // 找到 schema 字段（类型须为 serde_json::Value）
    let _schema_field = fields
        .iter()
        .find(|f| f.ident.as_ref().map(|i| i.to_string()) == Some("schema".into()))
        .ok_or_else(|| {
            syn::Error::new_spanned(tool_name, "struct must have a field named `schema: serde_json::Value`")
        })?;

    // 找到 read_only 字段（类型须为 bool）
    let _read_only_field = fields
        .iter()
        .find(|f| f.ident.as_ref().map(|i| i.to_string()) == Some("read_only".into()))
        .ok_or_else(|| {
            syn::Error::new_spanned(tool_name, "struct must have a field named `read_only: bool`")
        })?;

    let name_str = tool_name.to_string();

    let expanded = quote::quote! {
        impl ::llm::tool::ToolMeta for #tool_name {
            fn name(&self) -> &str {
                #name_str
            }

            fn description(&self) -> &str {
                #description
            }

            fn schema(&self) -> ::serde_json::Value {
                self.schema.clone()
            }

            fn read_only(&self) -> bool {
                self.read_only
            }
        }
    };
    Ok(expanded.into())
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
}
