//! Строки проверяются при сборке; вызовы не зависят от глобальной locale процесса.

use proc_macro::TokenStream;
use quote::quote;
use syn::{LitStr, parse_macro_input};
mod message;

#[proc_macro]
pub fn msg(input: TokenStream) -> TokenStream {
    let message::Input {
        id,
        plural,
        context,
        args,
    } = parse_macro_input!(input as message::Input);
    let context = context.map(|context| quote!(.context(#context)));
    let args = args.into_iter().map(|(name, value)| {
        if name == "count"
            && let Some(plural) = &plural
        {
            quote!(.plural(#plural, #value))
        } else {
            let name = name.to_string();
            quote!(.arg(#name, #value))
        }
    });
    quote!(::eva_sdk::Message::new(#id) #context #(#args)*).into()
}

#[proc_macro]
pub fn include_translations(input: TokenStream) -> TokenStream {
    let path = parse_macro_input!(input as LitStr);
    match catalogs(&path) {
        Ok(tokens) => tokens.into(),
        Err(error) => syn::Error::new(path.span(), error)
            .to_compile_error()
            .into(),
    }
}

fn catalogs(path: &LitStr) -> Result<proc_macro2::TokenStream, String> {
    let root = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?;
    let directory = std::path::PathBuf::from(root).join(path.value());
    let mut paths = std::fs::read_dir(&directory)
        .map_err(|e| format!("{}: {e}", directory.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    paths.retain(|p| p.extension().is_some_and(|s| s == "po"));
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{} contains no .po catalogs", directory.display()));
    }
    let mut catalogs = eva_protocol::types::text::Catalogs::default();
    let mut hash = blake3::Hasher::new();
    let mut includes = Vec::new();
    for path in paths {
        let source =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let locale = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("catalog file name is not UTF-8")?;
        let catalog = eva_protocol::types::text::Catalog::parse(&source, locale)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        catalogs.insert(catalog)?;
        hash.update(locale.as_bytes());
        hash.update(&[0]);
        hash.update(source.as_bytes());
        hash.update(&[0]);
        includes.push(path.to_str().ok_or("catalog path is not UTF-8")?.to_owned());
    }
    catalogs.revision = hash.finalize().to_hex().to_string();
    let serialized = serde_json::to_string(&catalogs).map_err(|e| e.to_string())?;
    Ok(quote!({
        #(const _: &str = include_str!(#includes);)*
        ::eva_sdk::__private::catalogs(#serialized)
    }))
}
