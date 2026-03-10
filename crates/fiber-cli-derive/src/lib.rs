//! Proc macro crate for generating CLI argument handling from fiber-json-types param structs.
//!
//! This crate provides the `#[derive(CliArgs)]` derive macro that generates:
//! 1. `fn augment_command(cmd: clap::Command) -> clap::Command` — adds clap `Arg`s
//! 2. `fn from_arg_matches(matches: &clap::ArgMatches) -> anyhow::Result<Self>` — parses args
//!
//! # Field type parsing categories
//!
//! The macro determines how to parse each field based on its type and `#[cli(...)]` attributes:
//!
//! - **`String`** — cloned directly from ArgMatches
//! - **`u64` / `u128`** — parsed via `.parse::<T>()`
//! - **`Hash256` / `Pubkey` / `Privkey`** and other `FromStr` types — parsed via `.parse::<T>()`
//! - **`#[cli(json)]`** — parsed via `serde_json::from_str()`
//! - **`#[cli(json_quoted)]`** — parsed via `serde_json::from_str(&format!("\"{}\"", s))`
//! - **`#[cli(serde_enum)]`** — parsed via `serde_json::from_value(Value::String(...))`
//! - **`#[cli(bool_flag)]`** — boolean flag with `.num_args(0..=1).default_missing_value("true")`
//! - **`Option<T>`** wrapper — makes the arg optional, applies inner-type parsing

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Data, DeriveInput, Expr, Fields, Ident, Lit, Meta, Token, Type};

/// Parsed representation of a single field's `#[cli(...)]` attributes.
#[derive(Debug, Default)]
struct CliFieldAttrs {
    required: bool,
    json: bool,
    json_quoted: bool,
    serde_enum: bool,
    bool_flag: bool,
    bool_default: Option<bool>,
    skip: bool,
    skip_expr: Option<Expr>,
    rename: Option<String>,
    help_override: Option<String>,
}

/// Individual cli attribute tokens.
#[derive(Debug)]
enum CliAttr {
    Required,
    Json,
    JsonQuoted,
    SerdeEnum,
    BoolFlag,
    Default(bool),
    Skip,
    SkipExpr(Expr),
    Rename(String),
    Help(String),
}

impl Parse for CliAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let ident: Ident = input.parse()?;
        match ident.to_string().as_str() {
            "required" => Ok(CliAttr::Required),
            "json" => Ok(CliAttr::Json),
            "json_quoted" => Ok(CliAttr::JsonQuoted),
            "serde_enum" => Ok(CliAttr::SerdeEnum),
            "bool_flag" => Ok(CliAttr::BoolFlag),
            "default" => {
                let _: Token![=] = input.parse()?;
                let lit: Lit = input.parse()?;
                match lit {
                    Lit::Bool(b) => Ok(CliAttr::Default(b.value)),
                    _ => Err(syn::Error::new_spanned(lit, "expected bool literal")),
                }
            }
            "skip" => {
                if input.peek(Token![=]) {
                    let _: Token![=] = input.parse()?;
                    let expr: Expr = input.parse()?;
                    Ok(CliAttr::SkipExpr(expr))
                } else {
                    Ok(CliAttr::Skip)
                }
            }
            "rename" => {
                let _: Token![=] = input.parse()?;
                let lit: Lit = input.parse()?;
                match lit {
                    Lit::Str(s) => Ok(CliAttr::Rename(s.value())),
                    _ => Err(syn::Error::new_spanned(lit, "expected string literal")),
                }
            }
            "help" => {
                let _: Token![=] = input.parse()?;
                let lit: Lit = input.parse()?;
                match lit {
                    Lit::Str(s) => Ok(CliAttr::Help(s.value())),
                    _ => Err(syn::Error::new_spanned(lit, "expected string literal")),
                }
            }
            _ => Err(syn::Error::new(
                ident.span(),
                format!("unknown cli attribute: {}", ident),
            )),
        }
    }
}

/// A list of cli attributes separated by commas.
struct CliAttrList {
    attrs: Vec<CliAttr>,
}

impl Parse for CliAttrList {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut attrs = Vec::new();
        while !input.is_empty() {
            attrs.push(input.parse()?);
            if !input.is_empty() {
                let _: Token![,] = input.parse()?;
            }
        }
        Ok(CliAttrList { attrs })
    }
}

impl CliFieldAttrs {
    fn from_syn_attrs(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut result = Self::default();
        for attr in attrs {
            if attr.path().is_ident("cli") {
                let parsed: CliAttrList = attr.parse_args()?;
                for cli_attr in parsed.attrs {
                    match cli_attr {
                        CliAttr::Required => result.required = true,
                        CliAttr::Json => result.json = true,
                        CliAttr::JsonQuoted => result.json_quoted = true,
                        CliAttr::SerdeEnum => result.serde_enum = true,
                        CliAttr::BoolFlag => result.bool_flag = true,
                        CliAttr::Default(val) => result.bool_default = Some(val),
                        CliAttr::Skip => result.skip = true,
                        CliAttr::SkipExpr(expr) => {
                            result.skip = true;
                            result.skip_expr = Some(expr);
                        }
                        CliAttr::Rename(name) => result.rename = Some(name),
                        CliAttr::Help(text) => result.help_override = Some(text),
                    }
                }
            }
        }
        Ok(result)
    }
}

/// Extract doc comments from syn attributes as a single string.
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

/// Convert a snake_case field name to kebab-case for CLI flags.
fn to_kebab_case(s: &str) -> String {
    s.replace('_', "-")
}

/// Determine if a type is `Option<T>` and return the inner type if so.
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

/// Get the last segment ident of a type path (e.g., `Hash256` from `crate::Hash256`).
fn type_last_ident(ty: &Type) -> Option<String> {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Represents a parsed struct field for CLI generation.
struct CliField<'a> {
    name: &'a Ident,
    ty: &'a Type,
    doc_comments: Vec<String>,
    cli_attrs: CliFieldAttrs,
}

/// Generate the code that builds a single `Arg` for a field.
fn gen_arg(field: &CliField) -> TokenStream2 {
    let field_name_str = field.name.to_string();
    let long_name = field
        .cli_attrs
        .rename
        .clone()
        .unwrap_or_else(|| to_kebab_case(&field_name_str));

    let is_optional = extract_option_inner(field.ty).is_some();

    // Determine help text
    let help = field.cli_attrs.help_override.clone().unwrap_or_else(|| {
        if field.doc_comments.is_empty() {
            field_name_str.clone()
        } else {
            field.doc_comments.join(" ")
        }
    });

    let required = field.cli_attrs.required || (!is_optional && !field.cli_attrs.bool_flag);

    if field.cli_attrs.bool_flag {
        let default_val = if field.cli_attrs.bool_default.unwrap_or(false) {
            "true"
        } else {
            "false"
        };
        if required && !is_optional {
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

/// Generate the parsing expression for a field's value from ArgMatches.
fn gen_parse_expr(field: &CliField) -> TokenStream2 {
    let field_name = field.name;
    let field_name_str = field.name.to_string();
    let ty = field.ty;

    // Handle skip
    if field.cli_attrs.skip {
        return if let Some(expr) = &field.cli_attrs.skip_expr {
            quote! { let #field_name: #ty = #expr; }
        } else {
            quote! { compile_error!("skipped field must have a default expression: #[cli(skip = expr)]"); }
        };
    }

    let is_option = extract_option_inner(ty).is_some();
    let inner_ty = extract_option_inner(ty).unwrap_or(ty);

    // Check attributes for parsing mode
    if field.cli_attrs.bool_flag {
        let default_val = field.cli_attrs.bool_default.unwrap_or(false);
        if is_option {
            return quote! {
                let #field_name: #ty = matches
                    .get_one::<String>(#field_name_str)
                    .map(|v| v.parse::<bool>().unwrap_or(#default_val));
            };
        } else {
            return quote! {
                let #field_name: #ty = matches
                    .get_one::<String>(#field_name_str)
                    .map(|v| v.parse::<bool>().unwrap_or(#default_val))
                    .unwrap_or(#default_val);
            };
        }
    }

    if field.cli_attrs.json {
        if is_option {
            return quote! {
                let #field_name: #ty = matches
                    .get_one::<String>(#field_name_str)
                    .map(|s| ::serde_json::from_str(s))
                    .transpose()
                    .map_err(|e| ::anyhow::anyhow!("Invalid {} JSON: {}", #field_name_str, e))?;
            };
        } else {
            return quote! {
                let #field_name: #ty = ::serde_json::from_str(
                    matches.get_one::<String>(#field_name_str).unwrap()
                )
                .map_err(|e| ::anyhow::anyhow!("Invalid {} JSON: {}", #field_name_str, e))?;
            };
        }
    }

    if field.cli_attrs.json_quoted {
        if is_option {
            return quote! {
                let #field_name: #ty = matches
                    .get_one::<String>(#field_name_str)
                    .map(|s| ::serde_json::from_str(&format!("\"{}\"", s)))
                    .transpose()
                    .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
            };
        } else {
            return quote! {
                let #field_name: #ty = ::serde_json::from_str(
                    &format!("\"{}\"", matches.get_one::<String>(#field_name_str).unwrap())
                )
                .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
            };
        }
    }

    if field.cli_attrs.serde_enum {
        if is_option {
            return quote! {
                let #field_name: #ty = matches
                    .get_one::<String>(#field_name_str)
                    .map(|s| ::serde_json::from_value(::serde_json::Value::String(s.clone())))
                    .transpose()
                    .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
            };
        } else {
            return quote! {
                let #field_name: #ty = ::serde_json::from_value(
                    ::serde_json::Value::String(matches.get_one::<String>(#field_name_str).unwrap().clone())
                )
                .map_err(|e| ::anyhow::anyhow!("Invalid {}: {}", #field_name_str, e))?;
            };
        }
    }

    // Auto-detect based on type name
    let inner_type_name = type_last_ident(inner_ty).unwrap_or_default();

    match inner_type_name.as_str() {
        "String" => {
            if is_option {
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
            if is_option {
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
        // Types that implement FromStr: Hash256, Pubkey, Privkey, etc.
        _ => {
            if is_option {
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

/// The `#[derive(CliArgs)]` derive macro.
///
/// Place this on a struct in `fiber-json-types` to auto-generate CLI argument
/// definitions and parsing. The generated code implements the `CliArgs` trait
/// with two methods:
///
/// - `augment_command(cmd: Command) -> Command` — adds clap `Arg`s to the command
/// - `from_arg_matches(matches: &ArgMatches) -> Result<Self>` — parses matches into the struct
///
/// # Example
///
/// ```ignore
/// #[derive(Serialize, Deserialize, Debug, CliArgs)]
/// pub struct RemoveWatchChannelParams {
///     /// The channel ID
///     pub channel_id: Hash256,
/// }
///
/// // Generated usage:
/// let cmd = RemoveWatchChannelParams::augment_command(Command::new("remove_watch_channel"));
/// let params = RemoveWatchChannelParams::from_arg_matches(&matches)?;
/// ```
#[proc_macro_derive(CliArgs, attributes(cli))]
pub fn derive_cli_args(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match generate_cli_args_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn generate_cli_args_impl(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    struct_name,
                    "CliArgs can only be derived for structs with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                struct_name,
                "CliArgs can only be derived for structs",
            ))
        }
    };

    // Parse fields into our internal representation
    let cli_fields: Vec<CliField> = fields
        .iter()
        .map(|f| {
            let cli_attrs = CliFieldAttrs::from_syn_attrs(&f.attrs)?;
            let doc_comments = extract_doc_comments(&f.attrs);
            Ok(CliField {
                name: f.ident.as_ref().unwrap(),
                ty: &f.ty,
                doc_comments,
                cli_attrs,
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;

    // Generate arg builders for non-skipped fields
    let arg_builders: Vec<TokenStream2> = cli_fields
        .iter()
        .filter(|f| !f.cli_attrs.skip)
        .map(gen_arg)
        .collect();

    // Generate parse expressions for all fields
    let parse_exprs: Vec<TokenStream2> = cli_fields.iter().map(gen_parse_expr).collect();

    // Generate struct field init (just field names, since we bind them as local vars)
    let field_inits: Vec<TokenStream2> = cli_fields
        .iter()
        .map(|f| {
            let name = f.name;
            quote! { #name }
        })
        .collect();

    let result = quote! {
        #[cfg(feature = "cli")]
        impl #struct_name {
            /// Add CLI arguments to a clap `Command`.
            pub fn augment_command(cmd: ::clap::Command) -> ::clap::Command {
                cmd #(#arg_builders)*
            }

            /// Parse a struct instance from clap `ArgMatches`.
            pub fn from_arg_matches(matches: &::clap::ArgMatches) -> ::anyhow::Result<Self> {
                #(#parse_exprs)*

                Ok(Self {
                    #(#field_inits,)*
                })
            }
        }
    };

    Ok(result)
}
