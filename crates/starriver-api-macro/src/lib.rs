//! `#[api_method]` attribute macro: automatically registers the host's
//! typed methods into the protocol catalog.
//!
//! Attached to methods of `impl HostRuntime`, it derives the parameter
//! JSON Schema from the function signature, generates a handler with
//! parameter extraction / error mapping / result serialization, and
//! generates the associated function `__api_<protocol_name>` (returning
//! `MethodSpec<Self>`), which the protocol catalog collects.
//!
//! ```ignore
//! impl HostRuntime {
//!     #[api_method("session.delete", permissions = ["session:write"])]
//!     pub fn delete_session(&self, session_id: String) -> anyhow::Result<()> { ... }
//! }
//! ```
//!
//! Supported parameter types: `String` / `Option<String>` / `bool` /
//! integers / floats / `Vec<String>` / `serde_json::Value`; protocol keys
//! are converted automatically from snake_case parameter names to
//! camelCase (`session_id` → `sessionId`). Returning `Result<(), E>`
//! serializes to `{}`; other `T` uses `serde_json::to_value`. Complex
//! results/streaming methods still use the hand-written catalog for now
//! (the `result`/`streaming` attributes will be extended later).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    parse_macro_input, AngleBracketedGenericArguments, FnArg, GenericArgument, Ident, ItemFn,
    LitStr, Pat, PathArguments, ReturnType, Token, Type, TypePath,
};

struct ApiMethodAttr {
    name: LitStr,
    permissions: Vec<LitStr>,
    streaming: bool,
    /// `result = "path::to::fn"`: custom conversion function from the
    /// host return value → protocol JSON.
    result: Option<syn::Path>,
}

impl Parse for ApiMethodAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: LitStr = input.parse()?;
        let mut permissions = Vec::new();
        let mut streaming = false;
        let mut result = None;
        while !input.is_empty() {
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
                continue;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "permissions" {
                let content;
                syn::bracketed!(content in input);
                while !content.is_empty() {
                    permissions.push(content.parse()?);
                    if content.peek(Token![,]) {
                        content.parse::<Token![,]>()?;
                    }
                }
            } else if key == "streaming" {
                let lit: syn::LitBool = input.parse()?;
                streaming = lit.value;
            } else if key == "result" {
                let path: LitStr = input.parse()?;
                result = Some(syn::parse_str(&path.value()).map_err(|_| {
                    syn::Error::new(
                        path.span(),
                        "result must be a Rust path like `crate::protocol_json::fn`",
                    )
                })?);
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "unknown #[api_method] attribute",
                ));
            }
        }
        Ok(Self {
            name,
            permissions,
            streaming,
            result,
        })
    }
}

/// Permission string → constant path (the protocol catalog's PERM_*
/// constant names are stable).
fn perm_ident(name: &str) -> syn::Result<Ident> {
    let ident = match name {
        "session:execute" => "PERM_SESSION_EXECUTE",
        "session:read" => "PERM_SESSION_READ",
        "session:write" => "PERM_SESSION_WRITE",
        "approval:respond" => "PERM_APPROVAL_RESPOND",
        "tools:read" => "PERM_TOOLS_READ",
        "models:read" => "PERM_MODELS_READ",
        "models:write" => "PERM_MODELS_WRITE",
        "credentials:read" => "PERM_CREDENTIALS_READ",
        "credentials:write" => "PERM_CREDENTIALS_WRITE",
        "plugins:read" => "PERM_PLUGINS_READ",
        "plugins:write" => "PERM_PLUGINS_WRITE",
        "skills:read" => "PERM_SKILLS_READ",
        "skills:write" => "PERM_SKILLS_WRITE",
        "workspaces:read" => "PERM_WORKSPACES_READ",
        "workspaces:write" => "PERM_WORKSPACES_WRITE",
        "agents:read" => "PERM_AGENTS_READ",
        "agents:write" => "PERM_AGENTS_WRITE",
        "memory:read" => "PERM_MEMORY_READ",
        "memory:write" => "PERM_MEMORY_WRITE",
        "mcp:read" => "PERM_MCP_READ",
        "mcp:write" => "PERM_MCP_WRITE",
        "scheduled:read" => "PERM_SCHEDULED_READ",
        "scheduled:write" => "PERM_SCHEDULED_WRITE",
        "jobs:read" => "PERM_JOBS_READ",
        "sources:read" => "PERM_SOURCES_READ",
        "sources:write" => "PERM_SOURCES_WRITE",
        "market:read" => "PERM_MARKET_READ",
        "market:write" => "PERM_MARKET_WRITE",
        "project:write" => "PERM_PROJECT_WRITE",
        "hooks:read" => "PERM_HOOKS_READ",
        "hooks:write" => "PERM_HOOKS_WRITE",
        "review:start" => "PERM_REVIEW_START",
        "attachments:read" => "PERM_ATTACHMENTS_READ",
        "attachments:write" => "PERM_ATTACHMENTS_WRITE",
        "ui:contrib" => "PERM_UI_CONTRIB",
        "shell:replace" => "PERM_SHELL_REPLACE",
        other => {
            return Err(syn::Error::new_spanned(
                other,
                format!("unknown permission `{other}` in #[api_method]"),
            ));
        }
    };
    Ok(format_ident!("{ident}"))
}

/// snake_case → camelCase (`session_id` → `sessionId`).
fn snake_to_camel(name: &str) -> String {
    name.split('_')
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                part.to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect()
}

/// Derives a JSON Schema fragment from a parameter type; returns
/// (schema expression, whether required, whether passed by reference).
fn type_schema(ty: &Type) -> syn::Result<(TokenStream2, bool, bool)> {
    // `&str` → string schema; pass by reference when calling the host method.
    if let Type::Reference(reference) = ty {
        let is_str = matches!(
            reference.elem.as_ref(),
            Type::Path(TypePath { path, .. })
                if path.segments.last().map(|s| s.ident == "str").unwrap_or(false)
        );
        if is_str {
            return Ok((
                quote!(::starriver_protocol::schema::string_schema()),
                true,
                true,
            ));
        }
        return Err(syn::Error::new_spanned(
            ty,
            "unsupported reference parameter type (only &str is supported)",
        ));
    }
    let Type::Path(TypePath { path, .. }) = ty else {
        return Err(syn::Error::new_spanned(ty, "unsupported parameter type"));
    };
    let last = path.segments.last().expect("type path non-empty");
    let name = last.ident.to_string();
    match name.as_str() {
        "String" => Ok((quote!(::starriver_protocol::schema::string_schema()), true, false)),
        "bool" => Ok((quote!(::starriver_protocol::schema::boolean_schema()), true, false)),
        "i64" | "u64" | "i32" | "u32" | "usize" => {
            Ok((quote!(::starriver_protocol::schema::integer_schema()), true, false))
        }
        "f64" | "f32" => Ok((quote!(::starriver_protocol::schema::number_schema()), true, false)),
        "Value" => Ok((quote!(::starriver_protocol::schema::any_schema()), true, false)),
        "Option" => {
            let inner = generic_type(&last.arguments)
                .ok_or_else(|| syn::Error::new_spanned(ty, "Option needs a type argument"))?;
            let (schema, _, is_ref) = type_schema(inner)?;
            if is_ref {
                return Err(syn::Error::new_spanned(ty, "Option<&str> is not supported"));
            }
            Ok((schema, false, false))
        }
        "Vec" => {
            let inner = generic_type(&last.arguments)
                .ok_or_else(|| syn::Error::new_spanned(ty, "Vec needs a type argument"))?;
            let (schema, _, is_ref) = type_schema(inner)?;
            if is_ref {
                return Err(syn::Error::new_spanned(ty, "Vec<&str> is not supported"));
            }
            Ok((
                quote!(::starriver_protocol::schema::array_schema(#schema)),
                true,
                false,
            ))
        }
        _ => Err(syn::Error::new_spanned(
            ty,
            format!("unsupported parameter type `{name}` — use String/Option/bool/int/float/Vec<String>/serde_json::Value"),
        )),
    }
}

fn generic_type<'a>(args: &'a PathArguments) -> Option<&'a Type> {
    match args {
        PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) => {
            args.iter().find_map(|arg| match arg {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
        }
        _ => None,
    }
}

/// Extracts T from `Result<T, E>`; returns (type tokens, whether unit).
fn parse_return(ty: &Type) -> syn::Result<(TokenStream2, bool)> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "api method must return Result<T, E>",
        ));
    };
    let last = path.segments.last().expect("type path non-empty");
    if last.ident != "Result" {
        return Err(syn::Error::new_spanned(
            ty,
            "api method must return Result<T, E>",
        ));
    }
    let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    else {
        return Err(syn::Error::new_spanned(ty, "Result needs type arguments"));
    };
    let ok_ty = args
        .iter()
        .find_map(|arg| match arg {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .ok_or_else(|| syn::Error::new_spanned(ty, "Result<T, E> missing T"))?;
    let is_unit = matches!(ok_ty, Type::Tuple(t) if t.elems.is_empty());
    Ok((quote!(#ok_ty), is_unit))
}

#[proc_macro_attribute]
pub fn api_method(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as ApiMethodAttr);
    let func = parse_macro_input!(item as ItemFn);
    match expand(attr, func) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(attr: ApiMethodAttr, func: ItemFn) -> syn::Result<TokenStream2> {
    let protocol_name = attr.name.value();
    if protocol_name.is_empty() || protocol_name.contains(char::is_whitespace) {
        return Err(syn::Error::new_spanned(
            &attr.name,
            "protocol method name must be a dotted identifier like `session.delete`",
        ));
    }
    let api_fn = format_ident!("__api_{}", protocol_name.replace('.', "_"));
    let method_ident = func.sig.ident.clone();
    let method_name = method_ident.to_string();

    let permissions: Vec<TokenStream2> = attr
        .permissions
        .iter()
        .map(|p| {
            let ident = perm_ident(&p.value())?;
            Ok(quote!(::starriver_protocol::permission::#ident))
        })
        .collect::<syn::Result<_>>()?;
    if permissions.is_empty() {
        return Err(syn::Error::new_spanned(
            &attr.name,
            "#[api_method] requires at least one permission",
        ));
    }

    let mut fields = Vec::new();
    let mut schema_entries = Vec::new();
    let mut required_keys = Vec::new();
    let mut arg_exprs = Vec::new();
    for arg in &func.sig.inputs {
        let FnArg::Typed(pat_ty) = arg else { continue };
        let ident = match pat_ty.pat.as_ref() {
            Pat::Ident(pat_ident) => pat_ident.ident.clone(),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "api method parameters must be plain identifiers",
                ));
            }
        };
        let key = snake_to_camel(&ident.to_string());
        let ty = &pat_ty.ty;
        let (schema, required, is_ref) = type_schema(ty)?;
        // A `&str` parameter cannot live in the generated params struct as-is
        // (an elided reference lifetime is E0106, and Deserialize cannot
        // borrow from the owned request Value) — own it as String and pass
        // `&__p.x` at the call site (which the arg expression already does).
        let type_text = ::quote::ToTokens::to_token_stream(ty).to_string();
        let is_str_ref = type_text.trim() == "&str";
        let field_ty = if is_str_ref {
            quote!(::std::string::String)
        } else {
            quote!(#ty)
        };
        fields.push(quote! {
            #[serde(rename = #key)]
            #ident: #field_ty
        });
        schema_entries.push(quote!((#key, #schema)));
        if required {
            required_keys.push(quote!(#key));
        }
        if is_ref {
            arg_exprs.push(quote!(&__p.#ident));
        } else {
            arg_exprs.push(quote!(__p.#ident));
        }
    }

    let (ok_ty, is_unit) = match &func.sig.output {
        ReturnType::Type(_, ty) => parse_return(ty)?,
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                "api method must declare a return type Result<T, E>",
            ));
        }
    };

    let streaming = attr.streaming;
    let result_schema = if streaming {
        quote!(__schema::object(
            &[("subscription", __schema::string_schema())],
            &["subscription"]
        ))
    } else {
        quote!(__schema::any_schema())
    };
    let handler = if streaming {
        // Streaming template: the host method returns
        // `Result<broadcast::Receiver<Value>, E>`; generate a subscription +
        // relay that forwards events; SESSION_BUSY maps to a stable error code.
        quote! {
            |call: ::starriver_protocol::registry::MethodCall<'_, Self>| {
                let __p: __ApiParams = ::serde_json::from_value(call.params.clone())
                    .map_err(|e| __ApiError::invalid_params(e.to_string()))?;
                let mut __receiver = call.host.#method_ident(#(#arg_exprs),*)
                    .map_err(|e| {
                        let __msg = e.to_string();
                        if let Some(rest) = __msg.strip_prefix("SESSION_BUSY:") {
                            __ApiError::session_busy(rest.trim())
                        } else {
                            __ApiError::internal(__msg)
                        }
                    })?;
                let __subscription = ::uuid::Uuid::new_v4().to_string();
                if let Some(emit) = call.emit.clone() {
                    let __runtime = call.host.runtime_handle().clone();
                    let __sub = __subscription.clone();
                    __runtime.spawn(async move {
                        let mut seq: u64 = 0;
                        loop {
                            let event = match __receiver.recv().await {
                                Ok(event) => event,
                                // Lagged means THIS consumer fell behind —
                                // the receiver stays usable; exiting the loop
                                // here silently starved the subscriber of its
                                // terminal event (its execute hangs forever).
                                Err(::tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(::tokio::sync::broadcast::error::RecvError::Closed) => break,
                            };
                            seq += 1;
                            let event_type = event
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("event")
                                .to_string();
                            emit(::starriver_protocol::EventEnvelope::new(
                                __sub.clone(),
                                seq,
                                event_type,
                                event,
                            ));
                        }
                    });
                }
                Ok(::serde_json::json!({ "subscription": __subscription }))
            }
        }
    } else {
        let result_expr = if let Some(path) = &attr.result {
            quote!(Ok((#path)(__result)))
        } else if is_unit {
            quote!(Ok(::serde_json::json!({})))
        } else {
            quote! {
                Ok(::serde_json::to_value(__result)
                    .map_err(|e| __ApiError::internal(e.to_string()))?)
            }
        };
        quote! {
            |call: ::starriver_protocol::registry::MethodCall<'_, Self>| {
                let __p: __ApiParams = ::serde_json::from_value(call.params.clone())
                    .map_err(|e| __ApiError::invalid_params(e.to_string()))?;
                let __result: #ok_ty = call.host.#method_ident(#(#arg_exprs),*)
                    .map_err(|e| __ApiError::internal(e.to_string()))?;
                #result_expr
            }
        }
    };
    Ok(quote! {
        #func

        #[doc = concat!("Protocol method `", #protocol_name, "` (macro-generated from `", #method_name, "`).")]
        pub fn #api_fn() -> ::starriver_protocol::registry::MethodSpec<Self> {
            use ::starriver_protocol::schema as __schema;
            use ::starriver_protocol::wire::ApiError as __ApiError;

            #[derive(::serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct __ApiParams {
                #(#fields),*
            }

            ::starriver_protocol::registry::MethodSpec::new(
                #protocol_name,
                __schema::object(&[#(#schema_entries),*], &[#(#required_keys),*]),
                #result_schema,
                &[#(#permissions),*],
                #streaming,
                #handler,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

    fn method(name: &str, signature: &str) -> ItemFn {
        let source = format!(
            "#[api_method({name})]\n{sig}\n{{ Ok(()) }}",
            sig = signature
        );
        syn::parse_str(&source).unwrap()
    }

    #[test]
    fn snake_to_camel_basic() {
        assert_eq!(snake_to_camel("session_id"), "sessionId");
        assert_eq!(snake_to_camel("session"), "session");
        assert_eq!(snake_to_camel("workspace_id"), "workspaceId");
        assert_eq!(snake_to_camel("a_b_c"), "aBC");
    }

    #[test]
    fn snake_to_camel_handles_trailing_and_double_underscores() {
        assert_eq!(snake_to_camel("id_"), "id");
        assert_eq!(snake_to_camel("a__b"), "aB");
        assert_eq!(snake_to_camel(""), "");
    }

    #[test]
    fn perm_ident_maps_known_permissions() {
        assert!(perm_ident("session:execute").is_ok());
        assert_eq!(
            perm_ident("session:execute").unwrap().to_string(),
            "PERM_SESSION_EXECUTE"
        );
        assert_eq!(
            perm_ident("attachments:write").unwrap().to_string(),
            "PERM_ATTACHMENTS_WRITE"
        );
        assert_eq!(
            perm_ident("shell:replace").unwrap().to_string(),
            "PERM_SHELL_REPLACE"
        );
    }

    #[test]
    fn perm_ident_rejects_unknown_permission() {
        let err = perm_ident("bogus:perm").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown permission"), "{message}");
        assert!(message.contains("bogus:perm"), "{message}");
    }

    #[test]
    fn type_schema_supports_known_scalar_types() {
        let (schema, required, is_ref) =
            type_schema(&parse_str::<Type>("String").unwrap()).unwrap();
        assert!(required);
        assert!(!is_ref);
        assert!(schema.to_string().contains("string_schema"));

        let (_, required, _) = type_schema(&parse_str::<Type>("Option<String>").unwrap()).unwrap();
        assert!(!required, "Option params are optional");

        let (schema, required, _) =
            type_schema(&parse_str::<Type>("Vec<String>").unwrap()).unwrap();
        assert!(required);
        assert!(schema.to_string().contains("array_schema"));

        let (schema, _, _) = type_schema(&parse_str::<Type>("i64").unwrap()).unwrap();
        assert!(schema.to_string().contains("integer_schema"));

        let (schema, _, _) = type_schema(&parse_str::<Type>("f64").unwrap()).unwrap();
        assert!(schema.to_string().contains("number_schema"));

        let (schema, _, _) = type_schema(&parse_str::<Type>("bool").unwrap()).unwrap();
        assert!(schema.to_string().contains("boolean_schema"));

        let (schema, _, _) = type_schema(&parse_str::<Type>("Value").unwrap()).unwrap();
        assert!(schema.to_string().contains("any_schema"));
    }

    #[test]
    fn type_schema_supports_str_reference_only() {
        let (_, required, is_ref) = type_schema(&parse_str::<Type>("&str").unwrap()).unwrap();
        assert!(required);
        assert!(is_ref);

        let err = type_schema(&parse_str::<Type>("&String").unwrap()).unwrap_err();
        assert!(err.to_string().contains("only &str"));

        let err = type_schema(&parse_str::<Type>("Option<&str>").unwrap()).unwrap_err();
        assert!(err.to_string().contains("Option<&str>"));

        let err = type_schema(&parse_str::<Type>("Vec<&str>").unwrap()).unwrap_err();
        assert!(err.to_string().contains("Vec<&str>"));
    }

    #[test]
    fn type_schema_rejects_unsupported_types() {
        let err = type_schema(&parse_str::<Type>("HashMap<String, String>").unwrap()).unwrap_err();
        assert!(err.to_string().contains("unsupported parameter type"));

        let err = type_schema(&parse_str::<Type>("Option").unwrap()).unwrap_err();
        assert!(err.to_string().contains("Option needs a type argument"));
    }

    #[test]
    fn parse_return_accepts_result_and_unit_result() {
        let (tokens, is_unit) =
            parse_return(&parse_str::<Type>("Result<String, anyhow::Error>").unwrap()).unwrap();
        assert!(!is_unit);
        assert_eq!(tokens.to_string(), "String");

        let (_, is_unit) =
            parse_return(&parse_str::<Type>("Result<(), anyhow::Error>").unwrap()).unwrap();
        assert!(is_unit);
    }

    #[test]
    fn parse_return_rejects_non_result() {
        let err = parse_return(&parse_str::<Type>("String").unwrap()).unwrap_err();
        assert!(err.to_string().contains("must return Result"));

        let err = parse_return(&parse_str::<Type>("Result").unwrap()).unwrap_err();
        assert!(err.to_string().contains("Result needs type arguments"));
    }

    #[test]
    fn attr_parses_name_permissions_streaming_and_result() {
        let attr: ApiMethodAttr = parse_str(
            r#""session.execute", permissions = ["session:write", "session:read"], streaming = true, result = "crate::protocol_json::skill_info_list""#,
        )
        .unwrap();
        assert_eq!(attr.name.value(), "session.execute");
        assert_eq!(attr.permissions.len(), 2);
        assert_eq!(attr.permissions[0].value(), "session:write");
        assert!(attr.streaming);
        let path = attr.result.unwrap();
        assert_eq!(
            quote!(#path).to_string(),
            "crate :: protocol_json :: skill_info_list"
        );
    }

    #[test]
    fn attr_rejects_unknown_key() {
        match parse_str::<ApiMethodAttr>(r#""x.y", bogus = 1"#) {
            Ok(_) => panic!("unknown attribute key must be rejected"),
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains("unknown #[api_method] attribute"),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn expand_rejects_invalid_protocol_names() {
        let func = method("\"\"", "pub fn f(&self) -> anyhow::Result<()>");
        let attr: ApiMethodAttr = parse_str("\"\"").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("dotted identifier"));

        let func = method("\"has space\"", "pub fn f(&self) -> anyhow::Result<()>");
        let attr: ApiMethodAttr = parse_str("\"has space\"").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("dotted identifier"));
    }

    #[test]
    fn expand_requires_at_least_one_permission() {
        let func = method(
            "\"session.delete\"",
            "pub fn delete(&self, session_id: String) -> anyhow::Result<()>",
        );
        let attr: ApiMethodAttr = parse_str("\"session.delete\"").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("at least one permission"));
    }

    #[test]
    fn expand_rejects_non_result_return() {
        let func = method(
            "\"session.delete\", permissions = [\"session:write\"]",
            "pub fn delete(&self, session_id: String) -> String",
        );
        let attr: ApiMethodAttr =
            parse_str("\"session.delete\", permissions = [\"session:write\"]").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("must return Result"));
    }

    #[test]
    fn expand_rejects_missing_return_type() {
        let source =
            "#[api_method(\"a.b\", permissions = [\"session:write\"])]\npub fn f(&self) {}";
        let func: ItemFn = syn::parse_str(source).unwrap();
        let attr: ApiMethodAttr = parse_str("\"a.b\", permissions = [\"session:write\"]").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("must declare a return type"));
    }

    #[test]
    fn expand_rejects_non_identifier_params() {
        let source = "#[api_method(\"a.b\", permissions = [\"session:write\"])]\npub fn f(&self, _: String) -> anyhow::Result<()> { Ok(()) }";
        let func: ItemFn = syn::parse_str(source).unwrap();
        let attr: ApiMethodAttr = parse_str("\"a.b\", permissions = [\"session:write\"]").unwrap();
        let err = expand(attr, func).unwrap_err();
        assert!(err.to_string().contains("plain identifiers"));
    }

    #[test]
    fn expand_generates_dotted_api_fn_name() {
        let func = method(
            "\"session.delete\", permissions = [\"session:write\"]",
            "pub fn delete(&self, session_id: String) -> anyhow::Result<()>",
        );
        let attr: ApiMethodAttr =
            parse_str("\"session.delete\", permissions = [\"session:write\"]").unwrap();
        let tokens = expand(attr, func).unwrap().to_string();
        assert!(tokens.contains("__api_session_delete"), "{tokens}");
        assert!(tokens.contains("sessionId"), "{tokens}");
        assert!(tokens.contains("deny_unknown_fields"), "{tokens}");
        assert!(tokens.contains("PERM_SESSION_WRITE"), "{tokens}");
    }

    #[test]
    fn expand_streaming_generates_subscription_result() {
        let func = method(
            "\"session.execute\", permissions = [\"session:execute\"], streaming = true",
            "pub fn execute(&self, session_id: String) -> anyhow::Result<tokio::sync::broadcast::Receiver<serde_json::Value>>",
        );
        let attr: ApiMethodAttr =
            parse_str("\"session.execute\", permissions = [\"session:execute\"], streaming = true")
                .unwrap();
        let tokens = expand(attr, func).unwrap().to_string();
        assert!(tokens.contains("subscription"), "{tokens}");
        assert!(tokens.contains("SESSION_BUSY"), "{tokens}");
    }
}
