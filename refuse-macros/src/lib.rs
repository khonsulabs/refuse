//! Macros for the [Refuse](https://github.com/khonsulabs/refuse) garbage
//! collector.

use std::collections::HashSet;

use attribute_derive::FromAttr;
use manyhow::manyhow;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{GenericParam, Generics, Lifetime, TraitBound};

/// Derives the `refuse::MapAs` trait for a given struct or enum.
///
/// This macro expects the identifier `refuse` to refer to the crate.
#[manyhow]
#[proc_macro_derive(MapAs, attributes(map_as))]
pub fn derive_map_as(input: syn::Item) -> manyhow::Result {
    match input {
        syn::Item::Struct(item) => derive_map_as_inner(&item.ident, &item.generics, &item.attrs),
        syn::Item::Enum(item) => derive_map_as_inner(&item.ident, &item.generics, &item.attrs),
        _ => manyhow::bail!("Collectable can only derived on structs and enums"),
    }
}

fn derive_map_as_inner(
    ident: &syn::Ident,
    generics: &syn::Generics,
    attrs: &[syn::Attribute],
) -> manyhow::Result {
    let (impl_gen, type_gen, where_clause) = generics.split_for_impl();
    let attr = MapAsAttr::from_attributes(attrs)?;

    match (&attr.target, &attr.map) {
        (Some(target), map_as) => {
            let map_as = if let Some(map_as) = map_as {
                quote!(#map_as)
            } else {
                quote!(self)
            };
            Ok(quote! {
                impl<#impl_gen> refuse::MapAs for #ident<#type_gen> #where_clause {
                    type Target = #target;

                    fn map_as(&self) -> &Self::Target {
                        #map_as
                    }
                }
            })
        }
        (None, None) => Ok(quote! {
            impl<#impl_gen> refuse::NoMapping for #ident<#type_gen> #where_clause {}
        }),
        (_, Some(map_as)) => {
            manyhow::bail!(map_as, "target must be specified when map is provided")
        }
    }
}

/// Derives the `refuse::Trace` trait for a given struct or enum.
///
/// This macro expects the identifier `refuse` to refer to the crate.
#[manyhow]
#[proc_macro_derive(Trace, attributes(trace))]
pub fn derive_trace(input: syn::Item) -> manyhow::Result {
    match input {
        syn::Item::Struct(item) => derive_struct_trace(item),
        syn::Item::Enum(item) => derive_enum_trace(item),
        _ => manyhow::bail!("Collectable can only derived on structs and enums"),
    }
}

fn field_accessor(field: &syn::Field, index: usize) -> TokenStream {
    if let Some(ident) = field.ident.clone() {
        quote!(#ident)
    } else {
        let index = proc_macro2::Literal::usize_unsuffixed(index);
        quote!(#index)
    }
}

fn derive_struct_trace(
    syn::ItemStruct {
        ident,
        mut generics,
        fields,
        ..
    }: syn::ItemStruct,
) -> manyhow::Result {
    require_trace_for_generics(&mut generics);
    let (impl_gen, type_gen, where_clause) = generics.split_for_impl();

    let mut fields_and_types = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let field_attr = TraceFieldAttr::from_attributes(&field.attrs)?;
        if !field_attr.ignore {
            let field_accessor = field_accessor(field, index);

            fields_and_types.push((field_accessor, field.ty.clone()));
        }
    }

    let types = fields_and_types
        .iter()
        .map(|(_, ty)| ty.clone())
        .collect::<HashSet<_>>();
    let type_mays = types
        .into_iter()
        .map(|ty| quote! {<#ty as refuse::Trace>::MAY_CONTAIN_REFERENCES});
    let may_contain_refs = quote! {
        #(#type_mays |)* false
    };

    let traces = fields_and_types.iter().map(|(field, _)| {
        quote! {refuse::Trace::trace(&self.#field, tracer)}
    });

    Ok(quote! {
        impl #impl_gen refuse::Trace for #ident #type_gen #where_clause {
            const MAY_CONTAIN_REFERENCES: bool = #may_contain_refs;

            fn trace(&self, tracer: &mut refuse::Tracer) {
                #(#traces;)*
            }
        }
    })
}

fn require_trace_for_generics(generics: &mut Generics) {
    for mut pair in generics.params.pairs_mut() {
        if let GenericParam::Type(t) = pair.value_mut() {
            t.bounds.push(syn::TypeParamBound::Trait(
                TraitBound::from_input(quote!(refuse::Trace)).unwrap(),
            ));
            t.bounds.push(syn::TypeParamBound::Trait(
                TraitBound::from_input(quote!(::std::marker::Send)).unwrap(),
            ));
            t.bounds.push(syn::TypeParamBound::Trait(
                TraitBound::from_input(quote!(::std::marker::Sync)).unwrap(),
            ));
            t.bounds.push(syn::TypeParamBound::Lifetime(
                Lifetime::from_input(quote!('static)).unwrap(),
            ));
        }
    }
}

fn derive_enum_trace(
    syn::ItemEnum {
        ident: enum_name,
        mut generics,
        variants,
        ..
    }: syn::ItemEnum,
) -> manyhow::Result {
    require_trace_for_generics(&mut generics);
    let (impl_gen, type_gen, where_clause) = generics.split_for_impl();

    let mut all_types = HashSet::new();
    let mut traces = Vec::new();

    for syn::Variant {
        ident,
        fields,
        attrs,
        ..
    } in &variants
    {
        let field_attr = TraceFieldAttr::from_attributes(attrs)?;
        let trace = match fields {
            syn::Fields::Named(fields) => {
                if field_attr.ignore {
                    quote!(Self::#ident { .. } => {})
                } else {
                    let mut field_names = Vec::new();
                    for field in &fields.named {
                        let field_attr = TraceFieldAttr::from_attributes(&field.attrs)?;
                        if !field_attr.ignore {
                            all_types.insert(field.ty.clone());

                            let name = field.ident.clone().expect("name missing");

                            field_names.push(name);
                        }
                    }
                    quote! {Self::#ident { #(#field_names,)* } => {
                        #(refuse::Trace::trace(#field_names, tracer);)*
                    }}
                }
            }
            syn::Fields::Unnamed(fields) => {
                if field_attr.ignore {
                    quote!(Self::#ident(..) => {})
                } else {
                    let mut field_names = Vec::new();
                    for (index, field) in fields.unnamed.iter().enumerate() {
                        let field_attr = TraceFieldAttr::from_attributes(&field.attrs)?;
                        if !field_attr.ignore {
                            all_types.insert(field.ty.clone());

                            field_names
                                .push(syn::Ident::new(&format!("f{index}"), Span::call_site()));
                        }
                    }
                    quote! {Self::#ident ( #(#field_names,)* ) => {
                        #(refuse::Trace::trace(#field_names, tracer);)*
                    }}
                }
            }
            syn::Fields::Unit => {
                quote! {Self::#ident => {}}
            }
        };
        traces.push(trace);
    }

    let type_mays = all_types
        .into_iter()
        .map(|ty| quote! {<#ty as refuse::Trace>::MAY_CONTAIN_REFERENCES});
    let may_contain_refs = quote! {
        #(#type_mays |)* false
    };
    let traces = if traces.is_empty() {
        TokenStream::default()
    } else {
        quote!(
            match self {
                #(#traces)*
            }
        )
    };
    Ok(quote! {
        impl #impl_gen refuse::Trace for #enum_name #type_gen #where_clause {
            const MAY_CONTAIN_REFERENCES: bool = #may_contain_refs;

            fn trace(&self, tracer: &mut refuse::Tracer) {
                #traces
            }
        }
    })
}

#[derive(FromAttr)]
#[attribute(ident = map_as)]
struct MapAsAttr {
    target: Option<syn::Type>,
    map: Option<syn::Expr>,
}

#[derive(FromAttr)]
#[attribute(ident = trace)]
struct TraceFieldAttr {
    ignore: bool,
}
