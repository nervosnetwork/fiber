//! Utilities for generating OpenRPC documents.
//!
//! This crate provides utilities to generate OpenRPC documents
//! in projects that uses the `jsonrpsee` crate for JSON-RPC method definitions.

use std::{error::Error, fs, path::Path};

use quote::ToTokens;

pub mod openrpc;

/// Generates a lookup table for the JSON-RPC methods defined in the given source file.
///
/// This function is meant to be used in the build script (`build.rs`) of a project.
pub fn generate_openrpc(
    json_rpc_methods_rs: &str,
    trait_names: &[&str],
    use_parent_module: bool,
    out_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    // Parse the source file containing the requested traits.
    let methods_rs = fs::read_to_string(json_rpc_methods_rs)?;
    let methods_ast = syn::parse_file(&methods_rs)?;

    // Collect references to all requested traits.
    let traits: Vec<&syn::ItemTrait> = trait_names
        .iter()
        .map(|name| {
            methods_ast
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Trait(item_trait) if item_trait.ident == *name => Some(item_trait),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("trait `{name}` must be present in {json_rpc_methods_rs}")
                })
        })
        .collect();

    let mut contents = "/// Lookup table for JSON-RPC methods.
#[allow(unused_qualifications)]
pub static METHODS: ::phf::Map<&str, openrpsee::openrpc::RpcMethod> = ::phf::phf_map! {
"
    .to_string();

    for item in traits.iter().flat_map(|tr| tr.items.iter()) {
        if let syn::TraitItem::Fn(method) = item {
            // Find methods via their `#[method(name = "command")]` attribute.
            let mut command = None;
            method
                .attrs
                .iter()
                .find(|attr| attr.path().is_ident("method"))
                .and_then(|attr| {
                    attr.parse_nested_meta(|meta| {
                        command = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                        Ok(())
                    })
                    .ok()
                });

            if let Some(command) = command {
                let module = match &method.sig.output {
                    syn::ReturnType::Type(_, ret) => match ret.as_ref() {
                        syn::Type::Path(type_path) => type_path.path.segments.first(),
                        _ => None,
                    },
                    _ => None,
                }
                .expect("required")
                .ident
                .to_string();

                // Extract the success type T from Result<T, E>.
                let result_type = match &method.sig.output {
                    syn::ReturnType::Type(_, ret) => match ret.as_ref() {
                        syn::Type::Path(type_path) => {
                            type_path.path.segments.first().and_then(|segment| {
                                if segment.ident == "Result" {
                                    if let syn::PathArguments::AngleBracketed(args) =
                                        &segment.arguments
                                    {
                                        if let Some(syn::GenericArgument::Type(ty)) =
                                            args.args.first()
                                        {
                                            return Some(ty.to_token_stream().to_string());
                                        }
                                    }
                                }
                                None
                            })
                        }
                        _ => None,
                    },
                    _ => None,
                };

                let params = method.sig.inputs.iter().filter_map(|arg| match arg {
                    syn::FnArg::Receiver(_) => None,
                    syn::FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
                        syn::Pat::Ident(pat_ident) => {
                            let parameter = pat_ident.ident.to_string();
                            let rust_ty = pat_type.ty.as_ref();

                            // If we can determine the parameter's optionality, do so.
                            let (param_ty, required) = match rust_ty {
                                syn::Type::Path(type_path) => {
                                    let is_standalone_ident =
                                        type_path.path.leading_colon.is_none()
                                            && type_path.path.segments.len() == 1;
                                    let first_segment = &type_path.path.segments[0];

                                    if first_segment.ident == "Option" && is_standalone_ident {
                                        // Strip the `Option<_>` for the schema type.
                                        let schema_ty = match &first_segment.arguments {
                                            syn::PathArguments::AngleBracketed(args) => {
                                                match args.args.first().expect("valid Option") {
                                                    syn::GenericArgument::Type(ty) => ty,
                                                    _ => panic!("Invalid Option"),
                                                }
                                            }
                                            _ => panic!("Invalid Option"),
                                        };
                                        (schema_ty, Some(false))
                                    } else if first_segment.ident == "Vec" {
                                        // We don't know whether the vec may be empty.
                                        (rust_ty, None)
                                    } else {
                                        (rust_ty, Some(true))
                                    }
                                }
                                _ => (rust_ty, Some(true)),
                            };

                            // Handle a few conversions we know we need.
                            let param_ty = param_ty.to_token_stream().to_string();
                            let schema_ty = match param_ty.as_str() {
                                "age :: secrecy :: SecretString" => "String".into(),
                                _ => param_ty,
                            };

                            Some((parameter, schema_ty, required))
                        }
                        _ => None,
                    },
                });

                // Collect doc comments for reuse in description and result.
                let mut doc_description = String::new();
                for attr in method
                    .attrs
                    .iter()
                    .filter(|attr| attr.path().is_ident("doc"))
                {
                    if let syn::Meta::NameValue(doc_line) = &attr.meta {
                        if let syn::Expr::Lit(docs) = &doc_line.value {
                            if let syn::Lit::Str(s) = &docs.lit {
                                // Trim the leading space from the doc comment line.
                                let line = s.value();
                                let trimmed_line = if line.is_empty() { &line } else { &line[1..] };

                                let escaped = trimmed_line.escape_default().collect::<String>();

                                doc_description.push_str(&escaped);
                                doc_description.push_str("\\n");
                            }
                        }
                    }
                }

                contents.push('"');
                contents.push_str(&command);
                contents.push_str("\" => openrpsee::openrpc::RpcMethod {\n");

                contents.push_str("    description: \"");
                contents.push_str(&doc_description);
                contents.push_str("\",\n");

                contents.push_str("    params: |_g| vec![\n");
                for (parameter, schema_ty, required) in params {
                    let param_upper = parameter.to_uppercase();

                    contents.push_str("        _g.param::<");
                    contents.push_str(&schema_ty);
                    contents.push_str(">(\"");
                    contents.push_str(&parameter);
                    if use_parent_module {
                        contents.push_str("\", super::");
                        contents.push_str(&module);
                    } else {
                        contents.push_str("\", crate::methods");
                    }
                    contents.push_str("::PARAM_");
                    contents.push_str(&param_upper);
                    contents.push_str("_DESC, ");
                    match required {
                        Some(required) => contents.push_str(&required.to_string()),
                        None => {
                            // Require a helper const to be present.
                            if use_parent_module {
                                contents.push_str("super::");
                            } else {
                                contents.push_str("crate::methods::");
                            }
                            contents.push_str(&module);
                            contents.push_str("::PARAM_");
                            contents.push_str(&param_upper);
                            contents.push_str("_REQUIRED");
                        }
                    }
                    contents.push_str("),\n");
                }
                contents.push_str("    ],\n");

                // Generate result with the actual return type extracted from
                // Result<T, E>, falling back to the old ResultType if extraction
                // failed.
                if let Some(ref result_ty) = result_type {
                    contents.push_str("    result: |g| g.result_schema::<");
                    contents.push_str(result_ty);
                    contents.push_str(">(\"");
                    contents.push_str(&command);
                    contents.push_str("_result\", \"");
                    contents.push_str(&doc_description);
                    contents.push_str("\"),\n");
                } else {
                    contents.push_str("    result: |g| g.result::<openrpsee::openrpc");
                    contents.push_str("::ResultType>(\"");
                    contents.push_str(&command);
                    contents.push_str("_result\"),\n");
                }

                contents.push_str("    deprecated: ");
                contents.push_str(
                    &method
                        .attrs
                        .iter()
                        .any(|attr| attr.path().is_ident("deprecated"))
                        .to_string(),
                );
                contents.push_str(",\n");

                contents.push_str("},\n");
            }
        }
    }

    contents.push_str("};");

    let rpc_openrpc_path = out_dir.join("rpc_openrpc.rs");
    fs::write(&rpc_openrpc_path, contents)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_result_type_extraction() {
        let test_rs = r#"
trait TestRpc {
    /// Get something cool
    #[method(name = "get_something")]
    async fn get_something(&self, params: MyParams) -> Result<MyResponse, ErrorObjectOwned>;

    /// Do action with no return
    #[method(name = "do_action")]
    async fn do_action(&self, params: MyParams) -> Result<(), ErrorObjectOwned>;
}
"#;
        let tmp = std::env::temp_dir().join("openrpsee_test");
        std::fs::create_dir_all(&tmp).unwrap();
        let test_file = tmp.join("test_rpc.rs");
        std::fs::write(&test_file, test_rs).unwrap();
        let out = tmp.join("output");
        std::fs::create_dir_all(&out).unwrap();

        generate_openrpc(test_file.to_str().unwrap(), &["TestRpc"], false, &out).unwrap();

        let generated = std::fs::read_to_string(out.join("rpc_openrpc.rs")).unwrap();
        println!("{}", generated);

        assert!(
            generated.contains("result_schema::<MyResponse>"),
            "Should use MyResponse as result type, got:\n{}",
            generated
        );
        assert!(
            generated.contains("result_schema::<()>"),
            "Should use () as result type, got:\n{}",
            generated
        );
        assert!(
            !generated.contains("::ResultType"),
            "Should NOT use hardcoded ResultType, got:\n{}",
            generated
        );
    }
}
