//! Proc macro to derive GCMetadata implementation for types.

extern crate proc_macro;

use proc_macro2::Span;
use quote::quote;
use std::collections::HashSet;
use synstructure::{decl_derive, AddBounds};

struct ArrayLike {
    pub length_field: Option<syn::Ident>,
    /// Getter for length field.
    pub length_getter: Option<syn::Expr>,
    /// Setter for length field.
    #[allow(dead_code)]
    pub length_setter: Option<syn::Expr>,
    pub element_type: syn::Type,
    pub data_field: Option<syn::Ident>,
    pub data_getter: Option<syn::Expr>,
    pub trace: bool,
}

impl Default for ArrayLike {
    fn default() -> Self {
        Self {
            length_field: None,
            length_getter: None,
            length_setter: None,
            element_type: syn::parse_str("u8").unwrap(),
            data_field: None,
            data_getter: None,
            trace: true,
        }
    }
}

fn find_arraylike(attrs: &[syn::Attribute]) -> syn::Result<Option<ArrayLike>> {
    let mut length_field = None;
    let mut length_getter = None;
    let mut length_setter = None;
    let mut element_type = None;
    let mut data_field = None;
    let mut data_getter = None;
    let mut trace = None;

    let mut has = false;
    for attr in attrs.iter() {
        if attr.meta.path().is_ident("arraylike") {
            has = true;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("trace") {
                    let value = meta.value()?.parse::<syn::LitBool>()?;
                    if value.value() {
                        trace = Some(true);
                    } else {
                        trace = Some(false);
                    }
                }

                if meta.path.is_ident("len") {
                    if length_field.is_some() || length_getter.is_some() || length_setter.is_some()
                    {
                        return Err(meta.error("multiple length fields found"));
                    }

                    // parse `len = field` or `len(getter = field)` or `len(setter = field)`
                    if meta.input.peek(syn::Token![=]) {
                        meta.input.parse::<syn::Token![=]>()?;
                        let field = meta.input.parse::<syn::Ident>()?;
                        if length_field.is_some() {
                            return Err(meta.error("multiple length fields found"));
                        }
                        length_field = Some(field);
                    } else if meta.input.peek(syn::token::Paren) {
                        meta.parse_nested_meta(|meta| {
                            if meta.path.is_ident("get") {
                                if length_getter.is_some() {
                                    return Err(meta.error("multiple length getters found"));
                                }

                                let field = meta.input.parse::<syn::Expr>()?;
                                length_getter = Some(field);
                            } else if meta.path.is_ident("set") {
                                if length_setter.is_some() {
                                    return Err(meta.error("multiple length setters found"));
                                }
                                let field = meta.input.parse::<syn::Expr>()?;
                                length_setter = Some(field);
                            } else {
                                return Err(meta.error("unknown length field"));
                            }
                            Ok(())
                        })?;
                    } else {
                        return Err(meta.error("unknown length attribute for #[arraylike]"));
                    }
                }

                if meta.path.is_ident("data") {
                    if data_field.is_some() || data_getter.is_some() {
                        return Err(meta.error("multiple data fields found"));
                    }
                    if meta.input.peek(syn::token::Paren) {
                        meta.parse_nested_meta(|meta| {
                            if meta.path.is_ident("get") {
                                if data_getter.is_some() {
                                    return Err(meta.error("multiple data getters found"));
                                }

                                let field = meta.input.parse::<syn::Expr>()?;
                                data_getter = Some(field);
                            } else {
                                return Err(meta.error("unknown data field"));
                            }
                            Ok(())
                        })?;
                    } else {
                        let field = meta.input.parse::<syn::Ident>()?;
                        data_field = Some(field);
                    }
                }

                if meta.path.is_ident("data_type") {
                    if element_type.is_some() {
                        return Err(meta.error("multiple data types found"));
                    }
                    meta.input.parse::<syn::Token![=]>()?;
                    let field = meta.input.parse::<syn::Type>()?;
                    element_type = Some(field);
                }

                Ok(())
            })?;
        }
    }

    if has {
        Ok(Some(ArrayLike {
            length_field,
            length_getter,
            length_setter,
            element_type: element_type
                .ok_or_else(|| syn::Error::new(Span::call_site(), "missing data_type"))?,
            data_field,
            data_getter,
            trace: trace.unwrap_or(true),
        }))
    } else {
        Ok(None)
    }
}

enum ObjectAlignment {
    Auto,
    Const(syn::Expr),
    #[allow(dead_code)]
    Compute(syn::Expr),
}

enum ObjectSize {
    Auto,
    Size(syn::Expr),
    Compute(syn::Expr),
}

enum ObjectTrace {
    NoTrace,
    Auto(bool),
    Trace(bool, syn::Expr),
}

struct GCMetadata {
    vm: syn::Type,
    alignment: ObjectAlignment,
    size: ObjectSize,
    trace: ObjectTrace,
}

fn find_gcmetadata(attrs: &[syn::Attribute]) -> syn::Result<GCMetadata> {
    let mut vm = None;
    let mut alignment = None;
    let mut size = None;
    let mut trace = None;

    let mut has = false;
    for attr in attrs.iter() {
        if attr.meta.path().is_ident("gcmetadata") {
            has = true;

            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("vm") {
                    if vm.is_some() {
                        return Err(meta.error("multiple vm fields found"));
                    }
                    meta.input.parse::<syn::Token![=]>()?;
                    let field = meta.input.parse::<syn::Type>()?;
                    vm = Some(field);
                }

                if meta.path.is_ident("align") {
                    if alignment.is_some() {
                        return Err(meta.error("multiple alignment fields found"));
                    }
                    if meta.input.peek(syn::token::Paren) {
                        let constant = meta.input.parse::<syn::Expr>()?;
                        alignment = Some(ObjectAlignment::Const(constant));
                    } else {
                        let _ = meta.input.parse::<syn::Token![=]>()?;
                        let field = meta.input.parse::<syn::Expr>()?;
                        alignment = Some(ObjectAlignment::Compute(field));
                    }
                }

                if meta.path.is_ident("size") {
                    if size.is_some() {
                        return Err(meta.error("multiple size fields found"));
                    }
                    if meta.input.peek(syn::token::Paren) {
                        let constant = meta.input.parse::<syn::Expr>()?;
                        size = Some(ObjectSize::Size(constant));
                    } else {
                        let _ = meta.input.parse::<syn::Token![=]>()?;
                        let field = meta.input.parse::<syn::Expr>()?;
                        size = Some(ObjectSize::Compute(field));
                    }
                }

                if meta.path.is_ident("trace") {
                    if trace.is_some() {
                        return Err(meta.error("multiple trace fields found"));
                    }
                    if meta.input.is_empty() {
                        trace = Some(ObjectTrace::Auto(false));
                    } else {
                        let _ = meta.input.parse::<syn::Token![=]>()?;
                        let field = meta.input.parse::<syn::Expr>()?;
                        trace = Some(ObjectTrace::Trace(false, field));
                    }
                } else if meta.path.is_ident("notrace") {
                    if trace.is_some() {
                        return Err(meta.error("multiple trace fields found"));
                    }
                    trace = Some(ObjectTrace::NoTrace);
                } else if meta.path.is_ident("scan") {
                    if trace.is_some() {
                        return Err(meta.error("multiple trace fields found"));
                    }
                    if meta.input.is_empty() {
                        trace = Some(ObjectTrace::Auto(true));
                    } else {
                        let _ = meta.input.parse::<syn::Token![=]>()?;
                        let field = meta.input.parse::<syn::Expr>()?;
                        trace = Some(ObjectTrace::Trace(true, field));
                    }
                }

                Ok(())
            })?;
        }
    }
    if !has {
        return Err(syn::Error::new(Span::call_site(), "missing gcmetadata"));
    }

    let vm = vm.ok_or_else(|| syn::Error::new(Span::call_site(), "VM for object not specified"))?;

    Ok(GCMetadata {
        vm,
        alignment: alignment.unwrap_or(ObjectAlignment::Auto),
        size: size.unwrap_or(ObjectSize::Auto),
        trace: trace.unwrap_or(ObjectTrace::Auto(false)),
    })
}
/*
#[proc_macro_derive(GCMetadata, attributes(arraylike, gcmetadata))]
pub fn derive_gcmetadata(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident;

    let arraylike = match find_arraylike(&input.attrs) {
        Ok(arraylike) => arraylike,
        Err(err) => return err.to_compile_error().into(),
    };

    let gc_meta = match find_gcmetadata(&input.attrs) {
        Ok(gc_meta) => gc_meta,
        Err(err) => return err.to_compile_error().into(),
    };


    let mut tracer = match gc_meta.trace {
        ObjectTrace::NoTrace => quote! { TraceCallback::None },
        ObjectTrace::Auto(scan_slots) => {
            let call = |typ: syn::Type, field: syn::Expr| {
                if scan_slots {
                    quote! {
                        <#typ as vmkit::mm::traits::Scan<_>>::scan_object(&#field, visitor);
                    }
                } else {
                    quote! {
                        <#typ as vmkit::mm::traits::Trace>::trace_object(&mut #field, visitor)
                    }
                }
            };


            let mut body = TokenStream::new();
            match input.data {
                Data::Enum(enumeration) => {
                    let mut match_: syn::ExprMatch = syn::parse_quote!(
                        match object.as_address().as_mut_ref::<#name>() {}
                    );

                    for variant in enumeration.variants {
                        let ident = variant.ident.clone();
                        let path = quote! { #name::#ident  }
                    }
                }

                Data::Struct(structure) => {

                }
            }
        }
    }

    TokenStream::new()
}
*/

decl_derive!([GCMetadata,attributes(gcmetadata, arraylike, ignore_trace)] => derive_gcmetadata);

fn derive_gcmetadata(s: synstructure::Structure<'_>) -> proc_macro2::TokenStream {
    let gc_metadata = match find_gcmetadata(&s.ast().attrs) {
        Ok(gc) => gc,
        Err(err) => return err.to_compile_error(),
    };
    let arraylike = match find_arraylike(&s.ast().attrs) {
        Ok(arraylike) => arraylike,
        Err(err) => return err.to_compile_error(),
    };

    let vm = &gc_metadata.vm;

    let name = s.ast().ident.clone();

    let mut fields_to_skip = HashSet::new();
    if let Some(arraylike) = arraylike.as_ref() {
        if let Some(field) = &arraylike.data_field {
            fields_to_skip.insert(field.clone());
        }
        if let Some(field) = &arraylike.length_field {
            fields_to_skip.insert(field.clone());
        }
    }

    let (trace_impl, trace_callback) = match gc_metadata.trace {
        ObjectTrace::NoTrace => (None, quote! { ::vmkit::prelude::TraceCallback::None }),
        ObjectTrace::Auto(scan_slots) => {
            let mut filtered = s.clone();
            filtered.filter(|bi| {
                !bi.ast()
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("ignore_trace"))
                    && (bi.ast().ident.is_none()
                        || !fields_to_skip.contains(bi.ast().ident.as_ref().unwrap()))
            });

            filtered.add_bounds(AddBounds::Fields);

            let trace_impl = if scan_slots {
                let trace_body = filtered.each(|bi| {
                    quote! {
                        mark(#bi, visitor);
                    }
                });

                let full_path = quote! { <#vm as vmkit::VirtualMachine>::Slot };

                let mut extra = quote! {
                    fn mark<T: ::vmkit::prelude::Scan<#full_path>>(t: &T, visitor: &mut dyn ::vmkit::prelude::SlotVisitor<#full_path>) {
                       ::vmkit::prelude::Scan::<#full_path>::scan_object(t, visitor);
                    }
                };
                if let Some(arraylike) = arraylike.as_ref().filter(|x| x.trace) {
                    let element = &arraylike.element_type;
                    let length = if let Some(getter) = &arraylike.length_getter {
                        quote! { #getter }
                    } else if let Some(field) = &arraylike.length_field {
                        quote! { self.#field as usize }
                    } else {
                        panic!("length field not found");
                    };

                    let data_ptr = if let Some(getter) = &arraylike.data_getter {
                        quote! { #getter }
                    } else if let Some(field) = &arraylike.data_field {
                        quote! { self.#field.as_mut_ptr().cast::<#element>() }
                    } else {
                        panic!("data field not found");
                    };

                    extra.extend(quote! {
                        let length = #length;
                        let data = #data_ptr;

                        for i in 0..length {
                            unsafe {
                                let ptr = data.add(i).as_ref().unwrap();
                                ::vmkit::prelude::Scan::<#full_path>::scan_object(ptr, visitor);
                            }
                        }
                    });
                }

                filtered.bound_impl(
                    quote! {::vmkit::prelude::Scan<#full_path> },
                    quote! {
                        #[inline]
                        fn scan_object(&self, visitor: &mut dyn ::vmkit::prelude::SlotVisitor<#full_path>) {
                            
                            let this = self;
                            #extra
                            match *self { #trace_body }
                        }
                    }
                )
            } else {
                filtered.bind_with(|_| synstructure::BindStyle::RefMut);
                let trace_body = filtered.each(|bi| {
                    quote! {
                        mark(#bi, visitor);
                    }
                });
                let mut extra = quote! {
                    #[inline]
                    fn mark<T: ::vmkit::prelude::Trace>(t: &mut T, visitor: &mut dyn ::vmkit::prelude::ObjectTracer) {
                        ::vmkit::prelude::Trace::trace_object(t, visitor);
                    }
                };
                if let Some(arraylike) = arraylike.as_ref().filter(|x| x.trace) {
                    let element = &arraylike.element_type;
                    let length = if let Some(getter) = &arraylike.length_getter {
                        quote! { #getter }
                    } else if let Some(field) = &arraylike.length_field {
                        quote! { self.#field as usize }
                    } else {
                        panic!("length field not found");
                    };

                    let data_ptr = if let Some(getter) = &arraylike.data_getter {
                        quote! { #getter }
                    } else if let Some(field) = &arraylike.data_field {
                        quote! { self.#field.as_mut_ptr().cast::<#element>() }
                    } else {
                        panic!("data field not found");
                    };

                    extra.extend(quote! {
                        let length = #length;
                        let data = #data_ptr;

                        for i in 0..length {
                            unsafe {
                                let ptr = data.add(i).as_mut().unwrap();
                                ::vmkit::prelude::Trace::trace_object(ptr, visitor);
                            }
                        }
                    });
                }

                filtered.bound_impl(
                    quote! {::vmkit::prelude::Trace },
                    quote! {
                        #[inline]
                        fn trace_object(&mut self, visitor: &mut dyn ::vmkit::prelude::ObjectTracer) {
                            let this = self;
                            #extra
                            match *self { #trace_body };
                            
                        }
                    }
                )
            };

            (
                Some(trace_impl),
                if scan_slots {
                    let full_path = quote! { <#vm as vmkit::VirtualMachine>::Slot };
                    quote! { ::vmkit::prelude::TraceCallback::ScanSlots(|object, visitor| unsafe {
                        let object = object.as_address().as_ref::<#name>();
                        ::vmkit::prelude::Scan::<#full_path>::scan_object(object, visitor);
                    }) }
                } else {
                    quote! { ::vmkit::prelude::TraceCallback::TraceObject(|object, visitor| unsafe {
                        let object = object.as_address().as_mut_ref::<#name>();
                        ::vmkit::prelude::Trace::trace_object(object, visitor);
                    }) }
                },
            )
        }

        ObjectTrace::Trace(scan_slots, expr) => {
            if scan_slots {
                (
                    None,
                    quote! {
                        ::vmkit::prelude::TraceCallback::ScanSlots(|object, visitor| unsafe {
                            let this = object.as_address().as_ref::<#name>();
                            #expr
                        })
                    },
                )
            } else {
                (
                    None,
                    quote! {
                        ::vmkit::prelude::TraceCallback::TraceObject(|object, visitor| unsafe {
                            let this = object.as_address().as_mut_ref::<#name>();
                            #expr
                        })
                    },
                )
            }
        }
    };

    let instance_size = match &gc_metadata.size {
        ObjectSize::Auto if arraylike.is_some() => quote! { 0 },
        ObjectSize::Size(_) if arraylike.is_some() => quote! { 0 },
        ObjectSize::Compute(_) => quote! { 0 },
        ObjectSize::Auto => quote! { ::std::mem::size_of::<#name>() },
        ObjectSize::Size(size) => quote! { #size },
    };

    let alignment = match &gc_metadata.alignment {
        ObjectAlignment::Auto => quote! { ::std::mem::align_of::<#name>() },
        ObjectAlignment::Const(expr) => quote! { #expr },
        ObjectAlignment::Compute(_) => quote! { 0 },
    };

    let compute_size = if let Some(arraylike) = arraylike.as_ref() {
        match gc_metadata.size {
            ObjectSize::Compute(expr) => quote! { Some(|object| unsafe {
                let this = object.as_address().as_ref::<#name>();
                #expr
            }) },
            _ => {
                let length = if let Some(getter) = &arraylike.length_getter {
                    quote! { #getter }
                } else if let Some(field) = &arraylike.length_field {
                    quote! { this.#field as usize }
                } else {
                    panic!("length field not found");
                };

                let element = &arraylike.element_type;
                quote! { Some(|object| unsafe {
                    let this = object.as_address().as_ref::<#name>();
                    let length = #length;


                    size_of::<#name>() + (length * ::std::mem::size_of::<#element>())
                }) }
            }
        }
    } else {
        match gc_metadata.size {
            ObjectSize::Compute(expr) => quote! { Some(|object| unsafe {
                let this = object.as_address().as_ref::<#name>();
                #expr
            }) },
            _ => quote! { None },
        }
    };

    let mut output = quote! {
        impl #name {
            pub fn gc_metadata() -> &'static ::vmkit::prelude::GCMetadata<#vm> {
                static METADATA: ::vmkit::prelude::GCMetadata<#vm> = ::vmkit::prelude::GCMetadata {
                    trace: #trace_callback,
                    instance_size: #instance_size,
                    compute_size: #compute_size,
                    alignment: #alignment,
                    compute_alignment: None,
                };
                &METADATA
            }
        }
    };

    if let Some(trace_impl) = trace_impl {
        output.extend(trace_impl);
    }

    output.into()
}

//mod bytecode;