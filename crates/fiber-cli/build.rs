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
// Command definitions parsing (from command_defs.rs)
// ---------------------------------------------------------------------------

/// Parsed representation of a single subcommand within a command group.
struct SubcommandDef {
    name: String,
    about: String,
    params_type: String, // "()" means no params
    result_type: String, // "()" means raw Value
}

/// What happens when no subcommand is given.
enum NoneAction {
    /// Print help and return Value::Null.
    Help,
    /// Call the named subcommand (which must have no params, i.e. params_type="()").
    Call(String),
    /// Call the named subcommand with Default::default() params.
    Default(String),
}

/// Parsed representation of one command group.
struct CommandGroupDef {
    group_name: String,
    about: String,
    none_action: NoneAction,
    subcommands: Vec<SubcommandDef>,
}

/// Extract a string literal from a syn Expr.
fn expr_to_string(expr: &Expr) -> Option<String> {
    if let Expr::Lit(lit_expr) = expr {
        if let Lit::Str(s) = &lit_expr.lit {
            return Some(s.value());
        }
    }
    None
}

/// Parse command_defs.rs and extract all command group definitions.
///
/// Each const is a 4-tuple: (group_name, about, none_action, &[(name, about, params, result)])
fn parse_command_defs(path: &Path) -> Vec<CommandGroupDef> {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
    let syntax = syn::parse_file(&content)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e));

    let mut groups = Vec::new();
    // Also track the order from ALL_COMMAND_GROUPS
    let mut ordered_names: Vec<String> = Vec::new();

    for item in &syntax.items {
        if let syn::Item::Const(c) = item {
            let const_name = c.ident.to_string();

            if const_name == "ALL_COMMAND_GROUPS" {
                // Parse the ordering array: &[&str]
                if let Expr::Reference(ref_expr) = &*c.expr {
                    if let Expr::Array(arr) = &*ref_expr.expr {
                        for elem in &arr.elems {
                            if let Some(s) = expr_to_string(elem) {
                                ordered_names.push(s);
                            }
                        }
                    }
                }
                continue;
            }

            if !const_name.ends_with("_COMMANDS") {
                continue;
            }

            // Parse the 4-tuple: (group_name, about, none_action, &[...])
            if let Expr::Tuple(tup) = &*c.expr {
                let elems: Vec<&Expr> = tup.elems.iter().collect();
                if elems.len() != 4 {
                    continue;
                }

                let group_name = expr_to_string(elems[0]).unwrap_or_default();
                let about = expr_to_string(elems[1]).unwrap_or_default();
                let none_action_str =
                    expr_to_string(elems[2]).unwrap_or_else(|| "help".to_string());

                let none_action = if none_action_str == "help" {
                    NoneAction::Help
                } else if let Some(method) = none_action_str.strip_prefix("call:") {
                    NoneAction::Call(method.to_string())
                } else if let Some(method) = none_action_str.strip_prefix("default:") {
                    NoneAction::Default(method.to_string())
                } else {
                    NoneAction::Help
                };

                // Parse the subcommands array: &[(name, about, params, result), ...]
                let mut subcommands = Vec::new();
                if let Expr::Reference(ref_expr) = elems[3] {
                    if let Expr::Array(arr) = &*ref_expr.expr {
                        for elem in &arr.elems {
                            if let Expr::Tuple(sub_tup) = elem {
                                let sub_elems: Vec<&Expr> = sub_tup.elems.iter().collect();
                                if sub_elems.len() == 4 {
                                    subcommands.push(SubcommandDef {
                                        name: expr_to_string(sub_elems[0]).unwrap_or_default(),
                                        about: expr_to_string(sub_elems[1]).unwrap_or_default(),
                                        params_type: expr_to_string(sub_elems[2])
                                            .unwrap_or_default(),
                                        result_type: expr_to_string(sub_elems[3])
                                            .unwrap_or_default(),
                                    });
                                }
                            }
                        }
                    }
                }

                groups.push(CommandGroupDef {
                    group_name,
                    about,
                    none_action,
                    subcommands,
                });
            }
        }
    }

    // Sort groups according to ALL_COMMAND_GROUPS ordering if available.
    if !ordered_names.is_empty() {
        groups.sort_by_key(|g| {
            let search = format!("{}_COMMANDS", g.group_name.to_uppercase());
            ordered_names
                .iter()
                .position(|n| *n == search)
                .unwrap_or(usize::MAX)
        });
    }

    groups
}

// ---------------------------------------------------------------------------
// Command module code generation
// ---------------------------------------------------------------------------

/// Generate the `command()` and `execute()` functions for one command group.
fn gen_command_module(group: &CommandGroupDef) -> proc_macro2::TokenStream {
    let group_name_str = &group.group_name;
    let about_str = &group.about;

    // Build the list of .subcommand() calls for the command() function.
    let subcommand_builders: Vec<proc_macro2::TokenStream> = group
        .subcommands
        .iter()
        .map(|sub| {
            let name = &sub.name;
            let about = &sub.about;
            if sub.params_type == "()" {
                // No params — plain Command
                quote! {
                    .subcommand(::clap::Command::new(#name).about(#about))
                }
            } else {
                // Has params — augment with CliArgs
                let params_ident =
                    syn::Ident::new(&sub.params_type, proc_macro2::Span::call_site());
                quote! {
                    .subcommand(#params_ident::augment_command(
                        ::clap::Command::new(#name).about(#about),
                    ))
                }
            }
        })
        .collect();

    // Build the match arms for the execute() function.
    let match_arms: Vec<proc_macro2::TokenStream> = group
        .subcommands
        .iter()
        .map(|sub| {
            let name = &sub.name;
            let has_params = sub.params_type != "()";
            let has_typed_result = sub.result_type != "()";

            if has_params && has_typed_result {
                // Pattern 1: Typed params + typed result
                let params_ident =
                    syn::Ident::new(&sub.params_type, proc_macro2::Span::call_site());
                let result_ident =
                    syn::Ident::new(&sub.result_type, proc_macro2::Span::call_site());
                quote! {
                    Some((#name, sub)) => {
                        let params = #params_ident::from_arg_matches(sub)?;
                        let result: #result_ident = client.call_typed(#name, &params).await?;
                        ::serde_json::to_value(result).map_err(Into::into)
                    }
                }
            } else if has_params {
                // Pattern 2: Typed params + raw Value result
                let params_ident =
                    syn::Ident::new(&sub.params_type, proc_macro2::Span::call_site());
                quote! {
                    Some((#name, sub)) => {
                        let params = #params_ident::from_arg_matches(sub)?;
                        let result: ::serde_json::Value = client.call_typed(#name, &params).await?;
                        Ok(result)
                    }
                }
            } else if has_typed_result {
                // Pattern 3: No params + typed result
                let result_ident =
                    syn::Ident::new(&sub.result_type, proc_macro2::Span::call_site());
                quote! {
                    Some((#name, _)) => {
                        let result: #result_ident = client.call_typed_no_params(#name).await?;
                        ::serde_json::to_value(result).map_err(Into::into)
                    }
                }
            } else {
                // Pattern 4: No params + raw Value result
                quote! {
                    Some((#name, _)) => {
                        let result: ::serde_json::Value = client.call_typed_no_params(#name).await?;
                        Ok(result)
                    }
                }
            }
        })
        .collect();

    // Generate the None arm based on none_action.
    let none_arm = match &group.none_action {
        NoneAction::Help => {
            quote! {
                None => {
                    command().print_help()?;
                    println!();
                    Ok(::serde_json::Value::Null)
                }
            }
        }
        NoneAction::Call(method_name) => {
            // Find the subcommand to determine its result type
            let sub = group
                .subcommands
                .iter()
                .find(|s| s.name == *method_name)
                .unwrap_or_else(|| {
                    panic!(
                        "NoneAction::Call references unknown subcommand '{}'",
                        method_name
                    )
                });
            let method_str = method_name.as_str();
            if sub.result_type == "()" {
                quote! {
                    None => {
                        let result: ::serde_json::Value = client.call_typed_no_params(#method_str).await?;
                        Ok(result)
                    }
                }
            } else {
                let result_ident =
                    syn::Ident::new(&sub.result_type, proc_macro2::Span::call_site());
                quote! {
                    None => {
                        let result: #result_ident = client.call_typed_no_params(#method_str).await?;
                        ::serde_json::to_value(result).map_err(Into::into)
                    }
                }
            }
        }
        NoneAction::Default(method_name) => {
            // Find the subcommand to determine its types
            let sub = group
                .subcommands
                .iter()
                .find(|s| s.name == *method_name)
                .unwrap_or_else(|| {
                    panic!(
                        "NoneAction::Default references unknown subcommand '{}'",
                        method_name
                    )
                });
            let method_str = method_name.as_str();
            let params_ident = syn::Ident::new(&sub.params_type, proc_macro2::Span::call_site());
            if sub.result_type == "()" {
                quote! {
                    None => {
                        let params = #params_ident::default();
                        let result: ::serde_json::Value = client.call_typed(#method_str, &params).await?;
                        Ok(result)
                    }
                }
            } else {
                let result_ident =
                    syn::Ident::new(&sub.result_type, proc_macro2::Span::call_site());
                quote! {
                    None => {
                        let params = #params_ident::default();
                        let result: #result_ident = client.call_typed(#method_str, &params).await?;
                        ::serde_json::to_value(result).map_err(Into::into)
                    }
                }
            }
        }
    };

    let error_msg = format!("Unknown {} subcommand. Use --help", group_name_str);

    quote! {
        pub fn command() -> ::clap::Command {
            ::clap::Command::new(#group_name_str)
                .about(#about_str)
                #(#subcommand_builders)*
        }

        pub async fn execute(
            client: &crate::rpc_client::RpcClient,
            matches: &::clap::ArgMatches,
        ) -> ::anyhow::Result<::serde_json::Value> {
            match matches.subcommand() {
                #(#match_arms)*
                #none_arm
                _ => Err(::anyhow::anyhow!(#error_msg)),
            }
        }
    }
}

/// Generate the complete commands_generated.rs file with all command modules.
fn gen_all_command_modules(groups: &[CommandGroupDef]) -> proc_macro2::TokenStream {
    // Collect all type names that need to be imported from fiber_json_types.
    let mut type_names: Vec<String> = Vec::new();
    for group in groups {
        for sub in &group.subcommands {
            if sub.params_type != "()" {
                type_names.push(sub.params_type.clone());
            }
            if sub.result_type != "()" {
                type_names.push(sub.result_type.clone());
            }
        }
    }
    type_names.sort();
    type_names.dedup();

    let type_idents: Vec<syn::Ident> = type_names
        .iter()
        .map(|n| syn::Ident::new(n, proc_macro2::Span::call_site()))
        .collect();

    // Generate per-module code, each wrapped in `pub mod group_name { ... }`.
    let modules: Vec<proc_macro2::TokenStream> = groups
        .iter()
        .map(|group| {
            let mod_ident = syn::Ident::new(&group.group_name, proc_macro2::Span::call_site());
            let module_body = gen_command_module(group);

            // Determine which types this module actually needs.
            let mut needed: Vec<&str> = Vec::new();
            for sub in &group.subcommands {
                if sub.params_type != "()" {
                    needed.push(&sub.params_type);
                }
                if sub.result_type != "()" {
                    needed.push(&sub.result_type);
                }
            }
            needed.sort();
            needed.dedup();

            let needs_cli_args = group.subcommands.iter().any(|s| s.params_type != "()");
            let cli_args_import = if needs_cli_args {
                quote! { use crate::cli_generated::CliArgs; }
            } else {
                quote! {}
            };

            let needed_idents: Vec<syn::Ident> = needed
                .iter()
                .map(|n| syn::Ident::new(n, proc_macro2::Span::call_site()))
                .collect();

            let type_import = if needed_idents.is_empty() {
                quote! {}
            } else {
                quote! { use fiber_json_types::{#(#needed_idents),*}; }
            };

            quote! {
                pub mod #mod_ident {
                    #cli_args_import
                    #type_import

                    #module_body
                }
            }
        })
        .collect();

    // Also generate the execute_command dispatcher and build_cli helpers.
    let dispatch_arms: Vec<proc_macro2::TokenStream> = groups
        .iter()
        .map(|group| {
            let name_str = &group.group_name;
            let mod_ident = syn::Ident::new(&group.group_name, proc_macro2::Span::call_site());
            quote! {
                Some((#name_str, sub)) => {
                    let result = #mod_ident::execute(client, sub).await?;
                    print_result(&result, raw, output_format, color);
                }
            }
        })
        .collect();

    let subcommand_registrations: Vec<proc_macro2::TokenStream> = groups
        .iter()
        .map(|group| {
            let mod_ident = syn::Ident::new(&group.group_name, proc_macro2::Span::call_site());
            quote! {
                .subcommand(#mod_ident::command())
            }
        })
        .collect();

    quote! {
        // Auto-generated by build.rs — do not edit.
        // This file provides command() and execute() functions for each command group,
        // plus the top-level execute_command() dispatcher.

        // Suppress unused import warnings — some modules may not need all imports.
        #[allow(unused_imports)]
        use fiber_json_types::{#(#type_idents),*};

        #(#modules)*

        /// Format and print the RPC response based on the output settings.
        pub fn print_result(value: &::serde_json::Value, raw_data: bool, output_format: &str, color: bool) {
            // Skip printing if the result is null (e.g., from help output)
            if value.is_null() {
                // Nothing to print.
            } else if raw_data {
                println!(
                    "{}",
                    ::serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                );
            } else {
                match output_format {
                    "json" => {
                        if color {
                            println!("{}", crate::colorize::colorize_json(value));
                        } else {
                            println!(
                                "{}",
                                ::serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                            );
                        }
                    }
                    "yaml" => {
                        if color {
                            println!("{}", crate::colorize::colorize_yaml(value));
                        } else {
                            println!(
                                "{}",
                                ::serde_yaml::to_string(value).unwrap_or_else(|_| value.to_string())
                            );
                        }
                    }
                    _ => {
                        if color {
                            println!("{}", crate::colorize::colorize_json(value));
                        } else {
                            println!(
                                "{}",
                                ::serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                            );
                        }
                    }
                }
            }
        }

        /// Execute an RPC subcommand and print its result.
        pub async fn execute_command(
            client: &crate::rpc_client::RpcClient,
            command: &::clap::ArgMatches,
            output_format: &str,
            color: bool,
        ) -> ::anyhow::Result<()> {
            let raw = client.raw_data();

            match command.subcommand() {
                #(#dispatch_arms)*
                _ => {
                    return Err(::anyhow::anyhow!(
                        "Unknown command. Use --help for available commands."
                    ));
                }
            }

            Ok(())
        }

        /// Register all command group subcommands onto a clap Command (for build_cli).
        pub fn register_subcommands(cmd: ::clap::Command) -> ::clap::Command {
            cmd #(#subcommand_registrations)*
        }
    }
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

    // Also re-run if command_defs.rs changes.
    let command_defs_path = PathBuf::from(&manifest_dir)
        .join("src")
        .join("command_defs.rs");
    println!("cargo::rerun-if-changed={}", command_defs_path.display());

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

    // Generate the CliArgs trait + impls output file.
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

    // Third: parse command_defs.rs and generate command modules.
    let groups = parse_command_defs(&command_defs_path);
    let commands_generated = gen_all_command_modules(&groups);
    let commands_out_path = PathBuf::from(&out_dir).join("commands_generated.rs");
    fs::write(&commands_out_path, commands_generated.to_string())
        .expect("Failed to write generated commands code");
}
