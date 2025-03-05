use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::Ident;

pub fn convert(size: usize, name: syn::Ident, typ: syn::Type) -> TokenStream {
    let sz = Ident::new(&format!("S{size}"), Span::call_site());
    quote! {
        Fits::<#typ, #sz>::convert(#name)
    }
}

pub fn check(size: usize, name: syn::Ident, typ: syn::Type) -> TokenStream {
    let sz = Ident::new(&format!("S{size}"), Span::call_site());
    quote! {
        Fits::<#typ, #sz>::check(#name)
    }
}

pub fn write(size: usize, name: syn::Ident, typ: syn::Type) -> TokenStream {
    let sz = Ident::new(&format!("S{size}"), Span::call_site());
    let cvt = convert(size, name, typ);
    quote! {
        generator.write(#cvt)
    }
}