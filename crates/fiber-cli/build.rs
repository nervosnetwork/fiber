//! Build script for fnn-cli: generates CLI argument handling code from
//! fiber-json-types source files, so that fiber-json-types itself doesn't
//! need any CLI-related dependencies or annotations.
//!
//! The generated code provides `augment_command` and `from_arg_matches`
//! methods on each param struct, identical to what the derive macro produced.
//!
//! **No external config file required.** All parse modes are inferred from
//! Rust types alone:
//!   - `String`                          → clone directly
//!   - `u64` / `u128`                    → `.parse::<T>()`
//!   - `bool` (always `Option<bool>`)    → bool_flag (default from serde attr)
//!   - `Script` / `OutPoint`             → json
//!   - `Vec<T>`                          → json
//!   - `EpochNumberWithFraction` / `JsonBytes` → json_quoted
//!   - Enums detected with `Deserialize` → serde_enum
//!   - `Hash256` / `Pubkey` / `Privkey` / `PeerId` → from_str
//!   - Everything else                   → json (safe default)

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use quote::quote;
use syn::{Expr, Fields, Lit, Meta, Type};

// ---------------------------------------------------------------------------
// Source parsing helpers
// ---------------------------------------------------------------------------

fn extract_doc_comments(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut docs = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let Meta::NameValue(nv) = &attr.meta {
                if let Expr::Lit(lit) = &nv.value {
                    if let Lit::Str(s) = &lit.lit {
                        docs.push(s.value().trim().to_string());
                    }
                }
            }
        }
    }
    docs
}

fn to_kebab_case(s: &str) -> String {
    s.replace('_', "-")
}

fn extract_option_inner(ty: &Type) -> Option<&Type> {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            if seg.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return Some(inner);
                    }
                }
            }
        }
    }
    None
}

fn type_last_ident(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Check if a derive attribute list contains a given trait name.
fn has_derive(attrs: &[syn::Attribute], trait_name: &str) -> bool {
    for attr in attrs {
        if attr.path().is_ident("derive") {
            if let Meta::List(ml) = &attr.meta {
                let tokens_str = ml.tokens.to_string();
                // Simple check: the trait name appears as a whole word in the derive list.
                // We split by non-alphanumeric chars and check for exact match.
                for token in tokens_str.split(|c: char| !c.is_alphanumeric() && c != '_') {
                    if token == trait_name {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Check if an item has a `#[serde(untagged)]` attribute.
fn has_serde_untagged(attrs: &[syn::Attribute]) -> bool {
    for attr in attrs {
        if attr.path().is_ident("serde") {
            if let Meta::List(ml) = &attr.meta {
                let tokens_str = ml.tokens.to_string();
                if tokens_str.contains("untagged") {
                    return true;
                }
            }
        }
    }
    false
}

/// Collect all enum names in a parsed file that derive `Deserialize` and
/// are suitable for simple string-based deserialization (i.e. NOT untagged).
/// Untagged enums need full JSON parsing and will fall through to "json" mode.
fn collect_serde_enums(syntax: &syn::File) -> HashSet<String> {
    let mut enums = HashSet::new();
    for item in &syntax.items {
        if let syn::Item::Enum(e) = item {
            if has_derive(&e.attrs, "Deserialize") && !has_serde_untagged(&e.attrs) {
                enums.insert(e.ident.to_string());
            }
        }
    }
    enums
}

/// Infer the parse mode from the type name.
///
/// The inference is designed so that **no external config file** is needed.
/// Types that implement `FromStr` are explicitly listed; everything else
/// defaults to `json` (safe: a missing `Deserialize` impl is a compile error,
/// whereas a missing `FromStr` would also be a compile error but with a more
/// confusing message).
fn infer_parse_mode(type_name: &str, serde_enums: &HashSet<String>) -> &'static str {
    match type_name {
        // Primitives
        "String" => "string",
        "u64" => "u64",
        "u128" => "u128",
        "bool" => "bool_flag",

        // Types that implement FromStr
        "Hash256" | "Pubkey" | "Privkey" | "PeerId" => "from_str",

        // CKB types serialized as JSON-quoted strings (e.g. "0x...")
        "EpochNumberWithFraction" | "JsonBytes" => "json_quoted",

        // Complex types that need JSON parsing
        "Script" | "OutPoint" => "json",
        "Vec" => "json",

        // Check if the type is a known serde enum from the source files
        other => {
            if serde_enums.contains(other) {
                "serde_enum"
            } else {
                // Default: assume complex struct/enum → json.
                // This is safer than from_str because a missing Deserialize impl
                // gives a clear compile error.
                "json"
            }
        }
    }
}

/// For `Option<bool>` fields used as bool_flag, determine the default value
/// when the flag is passed without an explicit value (e.g. `--public` vs
/// `--public false`).
///
/// The default is extracted from `#[serde(default = "fn_name")]` attributes:
/// if the function name contains "true", the default is `true`; otherwise `false`.
/// Fields without a serde default attribute default to `false`.
fn bool_flag_default_from_serde_attr(attrs: &[syn::Attribute]) -> bool {
    for attr in attrs {
        if attr.path().is_ident("serde") {
            if let Meta::List(ml) = &attr.meta {
                let tokens_str = ml.tokens.to_string();
                // Look for `default = "fn_name"` pattern.
                // The token stream renders as: default = "default_true"
                if let Some(pos) = tokens_str.find("default") {
                    let rest = &tokens_str[pos..];
                    // Extract the string literal value after `default =`
                    if let Some(start) = rest.find('"') {
                        if let Some(end) = rest[start + 1..].find('"') {
                            let fn_name = &rest[start + 1..start + 1 + end];
                            return fn_name.contains("true");
                        }
                    }
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

struct FieldInfo {
    name: String,
    ty_tokens: proc_macro2::TokenStream,
    inner_ty_tokens: proc_macro2::TokenStream,
    is_option: bool,
    doc: String,
    parse_mode: String,
    bool_default: Option<bool>,
    required: bool,
}

fn gen_arg_tokens(f: &FieldInfo) -> proc_macro2::TokenStream {
    let field_name_str = &f.name;
    let long_name = to_kebab_case(field_name_str);
    let help = if f.doc.is_empty() {
        field_name_str.clone()
    } else {
        f.doc.clone()
    };

    let required = f.required;

    if f.parse_mode == "bool_flag" {
        let default_val = if f.bool_default.unwrap_or(false) {
            "true"
        } else {
            "false"
        };
        if required && !f.is_option {
            quote! {
                .arg(
                    ::clap::Arg::new(#field_name_str)
                        .long(#long_name)
                        .required(true)
                        .num_args(0..=1)
                        .default_missing_value(#default_val)
                        .help(#help)
                )
            }
        } else {
            quote! {
                .arg(
                    ::clap::Arg::new(#field_name_str)
                        .long(#long_name)
                        .num_args(0..=1)
                        .default_missing_value(#default_val)
                        .help(#help)
                )
            }
        }
    } else if required {
        quote! {
            .arg(
                ::clap::Arg::new(#field_name_str)
                    .long(#long_name)
                    .required(true)
                    .help(#help)
            )
        }
    } else {
        quote! {
            .arg(
                ::clap::Arg::new(#field_name_str)
                    .long(#long_name)
                    .help(#help)
            )
        }
    }
}

fn gen_parse_tokens(f: &FieldInfo) -> proc_macro2::TokenStream {
    let field_name = syn::Ident::new(&f.name, proc_macro2::Span::call_site());
    let field_name_str = &f.name;
    let ty = &f.ty_tokens;
    let inner_ty = &f.inner_ty_tokens;

    match f.parse_mode.as_str() {
        "bool_flag" => {
            let default_val = f.bool_default.unwrap_or(false);
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|v| v.parse::<bool>().unwrap_or(#default_val));
                }
            } else {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|v| v.parse::<bool>().unwrap_or(#default_val))
                        .unwrap_or(#default_val);
                }
            }
        }
        "json" => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|s| ::serde_json::from_str(s))
                        .transpose()
                        .map_err(|e| ::anyhow::anyhow!("Invalid {} JSON: {}", #field_name_str, e))?;
                }
            } else {
                quote! {
                    let #field_name: #ty = ::serde_json::from_str(
                        matches.get_one::<String>(#field_name_str).unwrap()
                    )
                    .map_err(|e| ::anyhow::anyhow!("Invalid {} JSON: {}", #field_name_str, e))?;
                }
            }
        }
        "json_quoted" => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|s| ::serde_json::from_str(&format!("\"{}\"", s)))
                        .transpose()
                        .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            } else {
                quote! {
                    let #field_name: #ty = ::serde_json::from_str(
                        &format!("\"{}\"", matches.get_one::<String>(#field_name_str).unwrap())
                    )
                    .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            }
        }
        "serde_enum" => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|s| ::serde_json::from_value(::serde_json::Value::String(s.clone())))
                        .transpose()
                        .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            } else {
                quote! {
                    let #field_name: #ty = ::serde_json::from_value(
                        ::serde_json::Value::String(matches.get_one::<String>(#field_name_str).unwrap().clone())
                    )
                    .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            }
        }
        "string" => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches.get_one::<String>(#field_name_str).cloned();
                }
            } else {
                quote! {
                    let #field_name: #ty = matches.get_one::<String>(#field_name_str).unwrap().clone();
                }
            }
        }
        "u64" | "u128" => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|v| v.parse::<#inner_ty>()
                            .map_err(|_| ::anyhow::anyhow!("Invalid {}", #field_name_str)))
                        .transpose()?;
                }
            } else {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .unwrap()
                        .parse::<#inner_ty>()
                        .map_err(|_| ::anyhow::anyhow!("Invalid {}", #field_name_str))?;
                }
            }
        }
        // "from_str" and anything else
        _ => {
            if f.is_option {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .map(|s| s.parse::<#inner_ty>())
                        .transpose()
                        .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            } else {
                quote! {
                    let #field_name: #ty = matches
                        .get_one::<String>(#field_name_str)
                        .unwrap()
                        .parse()
                        .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
                }
            }
        }
    }
}

/// Convention: structs ending with `Params` are RPC param types that get
/// CLI argument generation.
fn should_generate_for(struct_name: &str) -> bool {
    struct_name.ends_with("Params")
}

fn process_source_file(
    path: &Path,
    serde_enums: &HashSet<String>,
) -> Vec<proc_macro2::TokenStream> {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("Failed to read {}: {}", path.display(), e);
    });

    let syntax = syn::parse_file(&content).unwrap_or_else(|e| {
        panic!("Failed to parse {}: {}", path.display(), e);
    });

    let mut impls = Vec::new();

    for item in &syntax.items {
        let item_struct = match item {
            syn::Item::Struct(s) => s,
            _ => continue,
        };

        let struct_name = item_struct.ident.to_string();
        if !should_generate_for(&struct_name) {
            continue;
        }

        let fields = match &item_struct.fields {
            Fields::Named(named) => &named.named,
            _ => continue,
        };

        let struct_ident = syn::Ident::new(&struct_name, proc_macro2::Span::call_site());

        let mut field_infos: Vec<FieldInfo> = Vec::new();

        for field in fields {
            let fname = field.ident.as_ref().unwrap().to_string();

            let ty = &field.ty;
            let is_option = extract_option_inner(ty).is_some();
            let inner_ty = extract_option_inner(ty).unwrap_or(ty);
            let inner_type_name = type_last_ident(inner_ty).unwrap_or_default();

            let parse_mode = infer_parse_mode(&inner_type_name, serde_enums).to_string();

            let doc = extract_doc_comments(&field.attrs).join(" ");

            let bool_default = if parse_mode == "bool_flag" {
                Some(bool_flag_default_from_serde_attr(&field.attrs))
            } else {
                None
            };

            let required = !is_option && parse_mode != "bool_flag";

            let ty_tokens = quote! { #ty };
            let inner_ty_tokens = quote! { #inner_ty };

            field_infos.push(FieldInfo {
                name: fname,
                ty_tokens,
                inner_ty_tokens,
                is_option,
                doc,
                parse_mode,
                bool_default,
                required,
            });
        }

        let arg_builders: Vec<_> = field_infos.iter().map(gen_arg_tokens).collect();
        let parse_exprs: Vec<_> = field_infos.iter().map(gen_parse_tokens).collect();
        let field_inits: Vec<proc_macro2::TokenStream> = field_infos
            .iter()
            .map(|f| {
                let ident = syn::Ident::new(&f.name, proc_macro2::Span::call_site());
                quote! { #ident }
            })
            .collect();

        impls.push(quote! {
            impl CliArgs for #struct_ident {
                fn augment_command(cmd: ::clap::Command) -> ::clap::Command {
                    cmd #(#arg_builders)*
                }

                fn from_arg_matches(matches: &::clap::ArgMatches) -> ::anyhow::Result<Self> {
                    #(#parse_exprs)*

                    Ok(Self {
                        #(#field_inits,)*
                    })
                }
            }
        });
    }

    impls
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let json_types_dir = PathBuf::from(&manifest_dir)
        .join("..")
        .join("fiber-json-types")
        .join("src");

    // Auto-discover all .rs source files in fiber-json-types/src/.
    // The `should_generate_for` filter ensures only structs ending with
    // `Params` get CLI generation, so scanning extra files is harmless.
    let mut source_files: Vec<PathBuf> = fs::read_dir(&json_types_dir)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", json_types_dir.display(), e))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    // Sort for deterministic output across builds.
    source_files.sort();

    // Tell cargo to re-run if any source file changes or a new file is added.
    println!("cargo::rerun-if-changed={}", json_types_dir.display());

    // First pass: collect all enum types with Deserialize across all source files.
    // This lets us auto-detect serde_enum fields without any config.
    let mut serde_enums = HashSet::new();
    for source_path in &source_files {
        let content = fs::read_to_string(source_path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {}", source_path.display(), e);
        });
        let syntax = syn::parse_file(&content).unwrap_or_else(|e| {
            panic!("Failed to parse {}: {}", source_path.display(), e);
        });
        serde_enums.extend(collect_serde_enums(&syntax));
    }

    // Second pass: generate CLI impls for all Params structs.
    let mut all_impls: Vec<proc_macro2::TokenStream> = Vec::new();

    for source_path in &source_files {
        let impls = process_source_file(source_path, &serde_enums);
        all_impls.extend(impls);
    }

    // Generate the output file
    let generated = quote! {
        // Auto-generated by build.rs — do not edit.
        // This file provides CliArgs trait impls for fiber-json-types param structs.

        use fiber_json_types::*;
        // Re-import external types used as direct field types in Params structs.
        // These are not re-exported by fiber_json_types::*.
        use ckb_jsonrpc_types::{EpochNumberWithFraction, JsonBytes, Script};

        /// Trait for types that can be used as CLI arguments.
        /// Generated implementations parse clap ArgMatches into the struct.
        pub trait CliArgs: Sized {
            /// Add CLI arguments to a clap `Command`.
            fn augment_command(cmd: ::clap::Command) -> ::clap::Command;
            /// Parse a struct instance from clap `ArgMatches`.
            fn from_arg_matches(matches: &::clap::ArgMatches) -> ::anyhow::Result<Self>;
        }

        #(#all_impls)*
    };

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = PathBuf::from(&out_dir).join("cli_generated.rs");
    fs::write(&out_path, generated.to_string()).expect("Failed to write generated CLI code");
}
