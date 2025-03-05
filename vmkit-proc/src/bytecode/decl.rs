use proc_macro2::TokenStream;
use quote::quote;

use super::fits;



/// A single opcode argument
pub struct Argument {
    pub name: syn::Ident,
    pub index: usize,
    pub optional: bool,
    pub ty: syn::Type
}

impl Argument {
    pub fn new(name: syn::Ident, index: usize, optional: bool, ty: syn::Type) -> Self {
        Self {
            name,
            index,
            optional,
            ty
        }
    }

    pub fn field(&self) -> TokenStream {
        let name = &self.name;
        let ty = &self.ty;
        quote! {
            #name: #ty
        }
    }

    pub fn create_param(&self) -> TokenStream {
        let name = &self.name;
        let ty = &self.ty;
        if self.optional {
            quote! {
                #name: Option<#ty>
            }
        } else {
            quote! {
                #name: #ty
            }
        }
    }

    pub fn field_name(&self) -> TokenStream {
        let name = &self.name;
        quote! {
            #name
        }
    }

    pub fn fits_check(&self, size: usize) -> TokenStream {
        fits::check(size, self.name.clone(), self.ty.clone())
    }

    pub fn fits_write(&self, size: usize) -> TokenStream {
        fits::write(size, self.name.clone(), self.ty.clone())
    }
}