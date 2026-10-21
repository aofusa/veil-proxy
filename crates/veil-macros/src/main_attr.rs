//! `#[veil::main]`: turns `async fn main()` into a synchronous `main` that runs its body.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{spanned::Spanned, Error, ItemFn};

pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    if !attr.is_empty() {
        return Err(Error::new(attr.span(), "#[veil::main] takes no arguments"));
    }
    let mut func: ItemFn = syn::parse2(item)?;
    if func.sig.asyncness.is_none() {
        return Err(Error::new(
            func.sig.fn_token.span(),
            "#[veil::main] requires `async fn main()`",
        ));
    }
    if !func.sig.inputs.is_empty() {
        return Err(Error::new(
            func.sig.inputs.span(),
            "main cannot take arguments",
        ));
    }
    func.sig.asyncness = None;
    let body = &func.block;
    let attrs = &func.attrs;
    let vis = &func.vis;
    let sig = &func.sig;
    Ok(quote! {
        #(#attrs)*
        #vis #sig {
            ::veil::api::block_on(async move #body)
        }
    })
}
