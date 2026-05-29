use proc_macro::TokenStream;
use quote::quote;
use std::path::Path;
use std::{
    collections::BTreeSet,
    env,
    fs::{self},
};
#[cfg(feature = "tracing")]
use syn::{Data, DeriveInput, Fields};
use syn::{ItemFn, LitBool, parse_macro_input};
use tauri_helper_core::{find_workspace_dir, get_workspace_pkg_name};

#[cfg(feature = "tracing")]
fn is_string_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "String";
        }
    }
    false
}

/// Derive macro for adding logging capabilities to enum variants.
///
/// This macro automatically implements `From<T>` for specified types,
/// emitting `tracing` logs whenever a conversion occurs.
///
/// # Example
///
/// ```rust
/// use tracing::error;
///
/// #[derive(WithLogging)]
/// enum Error {
///     #[logging_from(String)]
///     StringError(String),
///
///     #[logging_from(i32)]
///     IntError(i32),
///
///     StructError { code: i32, message: String },
/// }
/// ```
#[cfg(feature = "tracing")]
#[proc_macro_error]
#[proc_macro_derive(WithLogging, attributes(logging_from, no_from_string))]
pub fn derive_with_logging(input: TokenStream) -> TokenStream {
    use proc_macro_error::proc_macro_error;

    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let mut from_impls = vec![];

    if let Data::Enum(ref data_enum) = input.data {
        for variant in &data_enum.variants {
            let variant_name = &variant.ident;

            match &variant.fields {
                // Single unnamed field (e.g., SomeError(String))
                Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                    if let Some(attr) = variant
                        .attrs
                        .iter()
                        .find(|a| a.path().is_ident("logging_from"))
                    {
                        let convert_type = attr.parse_args::<syn::Type>().unwrap();
                        let field_type = &fields.unnamed.first().unwrap().ty;

                        let conversion = if is_string_type(field_type) {
                            quote! { value.to_string() }
                        } else {
                            quote! { value.into() }
                        };

                        let from_impl = quote! {
                            impl From<#convert_type> for #name {
                                fn from(value: #convert_type) -> Self {
                                    let converted_value: #field_type = #conversion;
                                    tracing::error!(
                                        "Error occurred: {} - {}",
                                        stringify!(#variant_name),
                                        converted_value
                                    );
                                    #name::#variant_name(converted_value)
                                }
                            }
                        };
                        from_impls.push(from_impl);
                    }
                }
                // Multiple unnamed fields (e.g., SomeError(String, i32))
                Fields::Unnamed(fields) => {
                    let field_types: Vec<_> = fields.unnamed.iter().map(|f| &f.ty).collect();
                    let field_names: Vec<_> = (0..field_types.len())
                        .map(|i| {
                            syn::Ident::new(&format!("field{}", i), proc_macro2::Span::call_site())
                        })
                        .collect();

                    let from_impl = quote! {
                        impl From<(#(#field_types),*)> for #name {
                            fn from(value: (#(#field_types),*)) -> Self {
                                let (#(#field_names),*) = value;
                                let err_str = format!("{:?}", (#(#field_names),*));
                                tracing::error!("Error occurred: {} - {}", stringify!(#variant_name), err_str);
                                #name::#variant_name(#(#field_names),*)
                            }
                        }
                    };
                    from_impls.push(from_impl);
                }

                // Struct-like fields (e.g., SomeError { message: String, code: i32 })
                Fields::Named(fields) => {
                    let field_names: Vec<_> = fields.named.iter().map(|f| &f.ident).collect();
                    let field_types: Vec<_> = fields.named.iter().map(|f| &f.ty).collect();

                    let from_impl = quote! {
                        impl From<(#(#field_types),*)> for #name {
                            fn from(value: (#(#field_types),*)) -> Self {
                                let (#(#field_names),*) = value;
                                let err_str = format!("{:?}", (#(&#field_names),*));
                                tracing::error!(
                                    "Error occurred: {} - {}",
                                    stringify!(#variant_name),
                                    err_str
                                );
                                #name::#variant_name { #(#field_names),* }
                            }
                        }
                    };
                    from_impls.push(from_impl);
                }

                _ => {}
            }
        }
    }

    let expanded = quote! {
        #(#from_impls)*
    };

    TokenStream::from(expanded)
}

/// Marks a Tauri command and registers it for automatic collection
#[proc_macro_attribute]
pub fn auto_collect_command(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let fn_name = input.sig.ident.to_string();

    if !fn_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        panic!("Function name `{}` is not a valid Rust identifier", fn_name);
    }

    // Returns the original function
    quote! { #input }.into()
}

fn is_specta_command_file(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with("_specta"))
}

/// Collects Tauri commands from the workspace's command list files in stable sorted order.
fn collect_commands(calling_crate: String, specta_only: bool) -> Vec<String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let workspace_root = find_workspace_dir(Path::new(&manifest_dir));
    let commands_dir = workspace_root.join("target").join("tauri_commands_list");

    let mut commands = BTreeSet::new();

    if let Ok(entries) = fs::read_dir(&commands_dir) {
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();

        for path in paths {
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("txt") {
                let is_specta_file = is_specta_command_file(&path);
                if specta_only != is_specta_file {
                    continue;
                }

                let crate_name = get_workspace_pkg_name();

                if let Ok(content) = fs::read_to_string(&path) {
                    for line in content.lines() {
                        let mut fn_name = line.trim().to_string();

                        // Strip prefix ONLY if it's the calling crate
                        if crate_name.replace("-", "_") == calling_crate.replace("-", "_")
                            && let Some(stripped) =
                                fn_name.strip_prefix(&format!("{}::", crate_name.replace("-", "_")))
                        {
                            fn_name = stripped.to_string();
                        }

                        if fn_name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
                        {
                            commands.insert(fn_name);
                        } else {
                            panic!("Invalid function name `{}` in command file", fn_name);
                        }
                    }
                }
            }
        }
    } else {
        eprintln!(
            "Warning: No commands directory found at {}",
            commands_dir.display()
        );
    }

    commands.into_iter().collect()
}

/// Generates the Specta collect_commands![] macro invocation with a list of all collected commands.
#[proc_macro]
pub fn specta_collect_commands(_item: TokenStream) -> TokenStream {
    let calling_crate = get_workspace_pkg_name();
    let commands = collect_commands(calling_crate, true);

    if commands.is_empty() {
        eprintln!(
            "Warning: No commands were collected. Ensure functions are annotated with `#[auto_collect_command]`."
        );
        return quote! { tauri_specta::collect_commands![] }.into();
    }

    let collected = commands.iter().map(|fn_name| {
        let path = syn::parse_str::<syn::Path>(fn_name).unwrap();
        quote!(#path)
    });

    quote! {
        tauri_specta::collect_commands![ #(#collected),* ]
    }
    .into()
}

/// Generates the Tauri generate_handler![] macro invocation with a list of all collected commands.
#[proc_macro]
pub fn tauri_collect_commands(_item: TokenStream) -> TokenStream {
    let calling_crate = get_workspace_pkg_name();
    let commands = collect_commands(calling_crate, false);

    if commands.is_empty() {
        eprintln!(
            "Warning: No commands were collected. Ensure functions are annotated with `#[auto_collect_command]`."
        );
        return quote! { tauri::generate_handler![] }.into();
    }

    let collected_paths = commands
        .iter()
        .map(|fn_name| syn::parse_str::<syn::Path>(fn_name).unwrap())
        .collect::<Vec<_>>();

    quote! {
        tauri::generate_handler![ #(#collected_paths),* ]
    }
    .into()
}

/// Generates an array of command names
///
/// If true is provided, as in `array_collect_commands(true)`, the macro will print the array, if nothing is provided, it won't.
#[proc_macro]
pub fn array_collect_commands(item: TokenStream) -> TokenStream {
    let print_arg = parse_macro_input!(item as Option<LitBool>);

    let should_print = print_arg.map(|lit| lit.value()).unwrap_or(false);

    let calling_crate = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".to_string());
    let commands = collect_commands(calling_crate, false);

    if commands.is_empty() {
        return quote! { [] }.into();
    }

    let collected = commands.iter().map(|fn_name| format!("\"{}\"", fn_name));
    let collected_str = collected.collect::<Vec<_>>().join(", ");

    let output = if should_print {
        quote! {
            {
                let arr = [ #collected_str ];
                arr
            }
        }
    } else {
        quote! {
            [ #collected_str ]
        }
    };

    output.into()
}
