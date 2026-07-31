//! `#[derive(FromDbRow)]` — the engine-neutral replacement for
//! `#[derive(sqlx::FromRow)]` (MAIN-327).
//!
//! A derive rather than hand-written impls, for one reason: there are ~47 DTOs,
//! and mapping is BY NAME, so a wrong or forgotten column is a runtime failure
//! rather than a compile error. Hand-writing forty-seven of those is forty-seven
//! chances to make a mistake the compiler cannot see, plus a second copy of
//! every field list to drift from the struct. Generated from the struct itself,
//! a field and its column cannot disagree unless someone says so on purpose.
//!
//! Re-exported as `nook_db::FromDbRow`, so callers depend on one crate.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr};

/// Per-field options, mirroring the `#[sqlx(…)]` set this replaces.
#[derive(Default)]
struct FieldOpts {
    rename: Option<String>,
    skip: bool,
    default: bool,
}

fn field_opts(attrs: &[syn::Attribute]) -> syn::Result<FieldOpts> {
    let mut o = FieldOpts::default();
    for attr in attrs {
        if !attr.path().is_ident("db") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                o.skip = true;
            } else if meta.path.is_ident("default") {
                o.default = true;
            } else if meta.path.is_ident("rename") {
                let s: LitStr = meta.value()?.parse()?;
                o.rename = Some(s.value());
            } else {
                return Err(meta.error("unknown #[db(…)] option: expected skip, default, rename"));
            }
            Ok(())
        })?;
    }
    Ok(o)
}

#[proc_macro_derive(FromDbRow, attributes(db))]
pub fn derive_from_db_row(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromDbRow works on structs with named fields",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromDbRow works on structs with named fields",
        ));
    };

    let mut assignments = Vec::new();
    for f in &fields.named {
        let ident = f.ident.as_ref().expect("named field");
        let opts = field_opts(&f.attrs)?;
        if opts.skip {
            // Not a column at all — a denormalised field the endpoints fill in.
            assignments.push(quote! { #ident: ::core::default::Default::default() });
            continue;
        }
        // A raw identifier's column is `type`, not `r#type`. `to_string()` on
        // the ident keeps the prefix, and the resulting lookup misses at RUNTIME
        // with `ColumnNotFound("r#type")` — sqlx's derive strips it, so dropping
        // this would have been a silent behaviour change for every `r#`-named
        // field in the tree.
        let column = opts
            .rename
            .unwrap_or_else(|| ident.to_string().trim_start_matches("r#").to_string());
        let read = if opts.default {
            quote! { row.get_or_default(#column)? }
        } else {
            quote! { row.get(#column)? }
        };
        assignments.push(quote! { #ident: #read });
    }

    Ok(quote! {
        impl #impl_generics ::nook_db::FromDbRow for #name #ty_generics #where_clause {
            fn from_db_row(row: &::nook_db::DbRow) -> ::core::result::Result<Self, ::nook_db::DbError> {
                ::core::result::Result::Ok(Self { #(#assignments),* })
            }
        }
    })
}
