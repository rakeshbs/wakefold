//! Procedural macros for Wakefold tools.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream as Tokens};
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, parse_macro_input};

/// Declares tool metadata on a named-field argument struct.
///
/// Use `#[tool(name = "...", description = "...")]` to provide the fields
/// shown to the model. Pair the derive with Serde's `Deserialize` and
/// Schemars's `JsonSchema`, then pass a typed handler to `wakefold::typed_tool`.
/// The optional `recovery` accepts `"retry_after_crash"` or `"stop_after_crash"`.
/// Tools default to `"stop_after_crash"`.
#[proc_macro_derive(WakefoldTool, attributes(tool))]
pub fn derive_wakefold_tool(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<Tokens> {
    let ident = &input.ident;
    if !matches!(&input.data, Data::Struct(data) if matches!(data.fields, Fields::Named(_))) {
        return Err(syn::Error::new(
            ident.span(),
            "WakefoldTool can only be derived for structs with named fields",
        ));
    }
    let (name, description, recovery) = tool_metadata(input)?;
    let wakefold = match crate_name("wakefold") {
        Ok(FoundCrate::Itself) => quote!(::wakefold),
        Ok(FoundCrate::Name(name)) => {
            let crate_ident = Ident::new_raw(&name, Span::call_site());
            quote!(::#crate_ident)
        }
        Err(error) => {
            return Err(syn::Error::new(
                ident.span(),
                format!("WakefoldTool requires the wakefold crate: {error}"),
            ));
        }
    };
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics #wakefold::ToolInput for #ident #type_generics #where_clause {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;
            const RECOVERY: #wakefold::ToolRecovery = #wakefold::ToolRecovery::#recovery;
        }
    })
}

fn tool_metadata(input: &DeriveInput) -> syn::Result<(LitStr, LitStr, Ident)> {
    let mut name = None;
    let mut description = None;
    let mut recovery = None;
    for attr in input
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("tool"))
    {
        attr.parse_nested_meta(|meta| {
            let property = if meta.path.is_ident("name") {
                &mut name
            } else if meta.path.is_ident("description") {
                &mut description
            } else if meta.path.is_ident("recovery") {
                &mut recovery
            } else {
                return Err(meta.error("expected name, description, or recovery"));
            };

            if property.is_some() {
                return Err(meta.error("tool property is specified more than once"));
            }
            *property = Some(meta.value()?.parse::<LitStr>()?);
            Ok(())
        })?;
    }

    let name = name.ok_or_else(|| syn::Error::new(input.ident.span(), "missing tool name"))?;
    let description = description
        .ok_or_else(|| syn::Error::new(input.ident.span(), "missing tool description"))?;
    let identifier = name.value();
    if identifier.is_empty()
        || identifier.len() > 64
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(syn::Error::new(
            name.span(),
            "tool name must be 1–64 ASCII letters, digits, underscores, or hyphens",
        ));
    }
    if description.value().trim().is_empty() {
        return Err(syn::Error::new(
            description.span(),
            "tool description must not be empty",
        ));
    }

    let variant = match recovery.as_ref().map(LitStr::value).as_deref() {
        None | Some("stop_after_crash") => "StopAfterCrash",
        Some("retry_after_crash") => "RetryAfterCrash",
        _ => {
            return Err(syn::Error::new_spanned(
                recovery,
                "recovery must be retry_after_crash or stop_after_crash",
            ));
        }
    };
    Ok((name, description, Ident::new(variant, input.ident.span())))
}

#[cfg(test)]
mod tests {
    use super::tool_metadata;

    #[test]
    fn reads_recovery_policy_and_defaults_to_stop() {
        let input = syn::parse_quote!(
            #[tool(name = "echo", description = "Echo")]
            struct Echo {
                message: String,
            }
        );
        assert_eq!(tool_metadata(&input).unwrap().2, "StopAfterCrash");
        for (recovery, variant) in [
            ("retry_after_crash", "RetryAfterCrash"),
            ("stop_after_crash", "StopAfterCrash"),
        ] {
            let input = syn::parse_quote!(
                #[tool(name = "echo", description = "Echo", recovery = #recovery)]
                struct Echo {
                    message: String,
                }
            );
            assert_eq!(tool_metadata(&input).unwrap().2, variant);
        }
    }

    #[test]
    fn rejects_unknown_and_repeated_recovery_attributes() {
        let input = syn::parse_quote!(
            #[tool(name = "echo", description = "Echo", recovery = "retry")]
            struct Echo {
                message: String,
            }
        );
        let Err(error) = tool_metadata(&input) else {
            panic!("unknown recovery policies must be rejected");
        };
        assert!(error.to_string().contains("recovery must be"));
        let input = syn::parse_quote!(
            #[tool(
                name = "echo",
                description = "Echo",
                recovery = "retry_after_crash",
                recovery = "stop_after_crash"
            )]
            struct Echo {
                message: String,
            }
        );
        let Err(error) = tool_metadata(&input) else {
            panic!("repeated recovery attributes must be rejected");
        };
        assert!(error.to_string().contains("more than once"));
    }
}
