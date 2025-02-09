#[proc_macro]
pub fn define_options(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let _input = proc_macro2::TokenStream::from(input);

    proc_macro::TokenStream::new()
}
