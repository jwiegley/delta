use convert_case::{Case, Casing};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident, Index};

fn has_attr(attrs: &[syn::Attribute], attr_name: &str) -> Option<syn::Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path.is_ident(attr_name))
        .peekable()
        .peek()
        .map(|x| (*x).clone())
}

#[derive(Clone)]
struct Attributes {
    visibility: syn::Visibility,
    type_name: syn::Ident,
    desc_name: syn::Ident,
    desc_type: proc_macro2::TokenStream,
    desc_body: proc_macro2::TokenStream,
    change_name: syn::Ident,
}

struct Generated {}

// jww (2021-10-30): Allow the Desc and Change suffixes to be configurable.

#[proc_macro_derive(
    Delta,
    attributes(
        describe_type,
        describe_body,
        no_description,
        compare_default,
        delta_public,
        delta_private,
        delta_ignore
    )
)]
pub fn delta_macro(input: TokenStream) -> TokenStream {
    impl_delta(parse_macro_input!(input as DeriveInput))
}

#[allow(clippy::cognitive_complexity)]
fn impl_delta(input: DeriveInput) -> TokenStream {
    let attrs = gather_attrs(&input);
    match &input.data {
        Data::Struct(st) => process_struct(&attrs, st),
        Data::Enum(en) => process_enum(&attrs, en),
        _ => {
            panic!("Delta derivation not yet implemented for unions");
        }
    }
}

fn gather_attrs(input: &DeriveInput) -> Attributes {
    let type_name = input.ident.clone();
    let desc_name = format_ident!("{}Desc", type_name);

    let visibility = if has_attr(&input.attrs, "delta_private").is_some() {
        syn::Visibility::Inherited
    } else if has_attr(&input.attrs, "delta_public").is_some() {
        syn::Visibility::Public(syn::VisPublic {
            pub_token: syn::token::Pub {
                span: Span::call_site(),
            },
        })
    } else {
        input.vis.clone()
    };

    let compare_default = has_attr(&input.attrs, "compare_default").is_some();

    let desc_type = if has_attr(&input.attrs, "no_description").is_some() {
        quote!(())
    } else if let Some(ty) = has_attr(&input.attrs, "describe_type").map(|x| {
        x.parse_args::<syn::Type>()
            .expect("Failed to parse \"describe_type\" attribute")
            .into_token_stream()
    }) {
        ty
    } else if compare_default {
        quote!(Self::Change)
    } else {
        quote!(Self)
    };

    let desc_body = if has_attr(&input.attrs, "no_description").is_some() {
        quote!(())
    } else if let Some(body) = has_attr(&input.attrs, "describe_body").map(|x| {
        x.parse_args::<syn::Expr>()
            .expect("Failed to parse \"describe_body\" attribute")
            .into_token_stream()
    }) {
        body
    } else if compare_default {
        quote!(#type_name::default().delta(self).unwrap_or_default())
    } else {
        quote!((*self).clone())
    };

    Attributes {
        visibility,
        type_name: type_name.clone(),
        desc_name,
        desc_type,
        desc_body,
        change_name: format_ident!("{}Change", type_name),
    }
}

fn process_struct(attrs: &Attributes, st: &syn::DataStruct) -> TokenStream {
    let Attributes {
        visibility,
        type_name,
        desc_name: _,
        desc_type,
        desc_body,
        change_name,
    } = attrs;

    let name_and_types = field_names_and_types(&st.fields);
    if name_and_types.is_empty() {
        let delta_impl = define_delta_impl(
            type_name,
            desc_type,
            desc_body,
            &quote!(()),
            &quote!(delta::Changed::Unchanged),
        );

        let gen = quote! {
            #delta_impl
        };
        gen.into()
    } else if name_and_types.len() == 1 {
        let FieldInfo {
            name: field_name,
            pascal_case: _,
            ty,
        } = &name_and_types[0];
        let ch = change_type(ty);
        let change_innards = vec![quote!(#ch)];
        let change_struct = definition(
            visibility,
            quote!(struct),
            change_name,
            false,
            change_innards,
        );
        let delta_impl = define_delta_impl(
            type_name,
            desc_type,
            desc_body,
            &quote!(#change_name),
            &quote! {
                self.#field_name.delta(&other.#field_name).map(#change_name)
            },
        );

        let gen = quote! {
            #change_struct
            #delta_impl
        };
        gen.into()
    } else {
        let change_struct = define_enum_from_fields(visibility, change_name, &st.fields);

        let delta_innards: Vec<proc_macro2::TokenStream> =
            name_and_types.iter().map(
                |FieldInfo {
                   name,
                   pascal_case,
                   ty: _,
                }|
                {
                    quote!(self.#name.delta(&other.#name).map(#change_name::#pascal_case).to_changes())
                }).collect();
        let delta_impl = define_delta_impl(
            type_name,
            desc_type,
            desc_body,
            &quote!(Vec<#change_name>),
            &quote! {
                let changes: Vec<#change_name> = vec![
                    #(#delta_innards),*
                ]
                    .into_iter()
                    .flatten()
                    .collect();
                if changes.is_empty() {
                    delta::Changed::Unchanged
                } else {
                    delta::Changed::Changed(changes)
                }
            },
        );

        let gen = quote! {
            #change_struct
            #delta_impl
        };
        gen.into()
    }
}

#[allow(clippy::cognitive_complexity)]
fn process_enum(attrs: &Attributes, en: &syn::DataEnum) -> TokenStream {
    let Attributes {
        visibility,
        type_name,
        desc_name,
        desc_type: _,
        desc_body: _,
        change_name,
    } = attrs;

    let mut desc_innards = Vec::<proc_macro2::TokenStream>::new();
    let mut match_innards = Vec::<proc_macro2::TokenStream>::new();
    let mut change_innards = Vec::<proc_macro2::TokenStream>::new();
    let mut is_unchanged_innards = Vec::<proc_macro2::TokenStream>::new();
    let mut delta_innards = Vec::<proc_macro2::TokenStream>::new();

    for variant in en.variants.iter() {
        // jww (2021-10-30): Also need to check for delta_ignore on the
        // variant's fields.
        if has_attr(&variant.attrs, "delta_ignore").is_none() {
            let vname = &variant.ident;

            // jww (2021-10-30): This is what needs to happen, rather than the
            // complicated code below: Using the name of the original struct
            // (Foo), the name of the variant (Bar), and the set of fields for
            // that variant, define a structure named `FooBar` that gives a
            // concrete type for that variant's fields. Then the Change for
            // that variant is Bar(<FooBar as Delta>::Change), after deriving
            // Delta for the generated struct.

            let _fields_change_struct = create_mirror_struct(
                visibility,
                &format_ident!("{}{}", type_name, vname),
                &"Change",
                &variant.fields,
                false,
            );

            match &variant.fields {
                Fields::Named(named) => {
                    let desc_decls: Vec<proc_macro2::TokenStream> = named
                        .named
                        .iter()
                        .map(|field| {
                            let ident = &field.ident;
                            let ty = desc_type(&field.ty);
                            quote!(#ident: #ty)
                        })
                        .collect();
                    desc_innards.push(quote!(#vname { #(#desc_decls),* }));

                    let field_decls: Vec<proc_macro2::TokenStream> = named
                        .named
                        .iter()
                        .map(|field| {
                            let ident = &field.ident;
                            let ty = change_type(&field.ty);
                            quote!(#ident: delta::Changed<#ty>)
                        })
                        .collect();
                    change_innards.push(quote!(#vname { #(#field_decls),* }));

                    let idents: Vec<&syn::Ident> = named
                        .named
                        .iter()
                        .map(|field| field.ident.as_ref().unwrap())
                        .collect();
                    let vars: Box<dyn Fn(&str) -> Vec<proc_macro2::TokenStream>> =
                        Box::new(|prefix| {
                            named
                                .named
                                .iter()
                                .zip(0usize..)
                                .map(|(_field, index)| {
                                    let var = format_ident!("{}_var{}", prefix, index);
                                    quote!(#var)
                                })
                                .collect()
                        });
                    let self_vars = vars("self");
                    let other_vars = vars("other");

                    match_innards.push(quote! {
                        #type_name::#vname { #(#idents: #self_vars),* } =>
                            #desc_name::#vname {
                                #(#idents: #self_vars.describe()),*
                            }
                    });

                    is_unchanged_innards.push(quote! {
                        #change_name::#vname { #(#idents: #self_vars),* } =>
                            vec![#(#self_vars.is_unchanged()),*].into_iter().all(std::convert::identity)
                    });

                    delta_innards.push(quote! {
                        (#type_name::#vname { #(#idents: #self_vars),* },
                         #type_name::#vname { #(#idents: #other_vars),* }) => {
                            let change = #change_name::#vname {
                                #(#idents: #self_vars.delta(&#other_vars)),*
                            };
                            if change.is_unchanged() {
                                delta::Changed::Unchanged
                            } else {
                                delta::Changed::Changed(delta::EnumChange::SameVariant(change))
                            }
                        }
                    });
                }
                Fields::Unnamed(unnamed) => {
                    let desc_decls: Vec<proc_macro2::TokenStream> = unnamed
                        .unnamed
                        .iter()
                        .map(|field| {
                            let ty = desc_type(&field.ty);
                            quote!(#ty)
                        })
                        .collect();
                    desc_innards.push(quote!(#vname(#(#desc_decls),*)));

                    let field_decls: Vec<proc_macro2::TokenStream> = unnamed
                        .unnamed
                        .iter()
                        .map(|field| {
                            let ty = change_type(&field.ty);
                            quote!(delta::Changed<#ty>)
                        })
                        .collect();
                    change_innards.push(quote!(#vname(#(#field_decls),*)));

                    let vars: Box<dyn Fn(&str) -> Vec<proc_macro2::TokenStream>> =
                        Box::new(|prefix| {
                            unnamed
                                .unnamed
                                .iter()
                                .zip(0usize..)
                                .map(|(_field, index)| {
                                    let var: syn::Ident = Ident::new(
                                        &format!("{}_var{}", prefix, index),
                                        Span::call_site(),
                                    );
                                    quote!(#var)
                                })
                                .collect()
                        });
                    let self_vars = vars("self");
                    let other_vars = vars("other");

                    match_innards.push(quote! {
                        #type_name::#vname(#(#self_vars),*) =>
                            #desc_name::#vname(#(#self_vars.describe()),*)
                    });

                    is_unchanged_innards.push(quote! {
                        #change_name::#vname(#(#self_vars),*) =>
                            vec![#(#self_vars.is_unchanged()),*].into_iter().all(std::convert::identity)
                    });

                    delta_innards.push(quote! {
                        (#type_name::#vname(#(#self_vars),*),
                         #type_name::#vname(#(#other_vars),*)) => {
                            let change = #change_name::#vname(#(#self_vars.delta(&#other_vars)),*);
                            if change.is_unchanged() {
                                delta::Changed::Unchanged
                            } else {
                                delta::Changed::Changed(delta::EnumChange::SameVariant(change))
                            }
                        }
                    });
                }
                Fields::Unit => {
                    desc_innards.push(quote!(#vname));
                    change_innards.push(quote!(#vname));
                    match_innards.push(quote!(#type_name::#vname => #desc_name::#vname));
                    is_unchanged_innards.push(quote!(
                        #change_name::#vname => true
                    ));
                    delta_innards.push(
                        quote!((#type_name::#vname, #type_name::#vname) => delta::Changed::Unchanged),
                    );
                }
            }
        }
    }

    delta_innards.push(quote! {
        (_, _) => delta::Changed::Changed(
            delta::EnumChange::DiffVariant(
                self.describe(), other.describe()))
    });

    let desc_struct = definition(visibility, quote!(enum), desc_name, false, desc_innards);
    let change_struct = definition(visibility, quote!(enum), change_name, false, change_innards);

    let delta_impl = define_delta_impl(
        type_name,
        &quote!(#desc_name),
        &quote! {
            match self {
                #(#match_innards),*
            }
        },
        &quote!(delta::EnumChange<Self::Desc, #change_name>),
        &quote! {
            match (self, other) {
                #(#delta_innards),*
            }
        },
    );

    let gen = quote! {
        #desc_struct
        #change_struct

        impl #change_name {
            #visibility fn is_unchanged(&self) -> bool {
                match self {
                    #(#is_unchanged_innards),*
                }
            }
        }

        #delta_impl
    };
    gen.into()
}

struct FieldInfo<'a> {
    name: Box<dyn ToTokens>,
    pascal_case: syn::Ident,
    ty: &'a syn::Type,
}

fn field_names_and_types(fields: &syn::Fields) -> Vec<FieldInfo> {
    let mut result = Vec::new();
    match fields {
        Fields::Named(named) => {
            for field in named.named.iter() {
                if has_attr(&field.attrs, "delta_ignore").is_none() {
                    let name: &syn::Ident = field.ident.as_ref().unwrap();
                    let capitalized: syn::Ident =
                        Ident::new(&name.to_string().to_case(Case::Pascal), Span::call_site());
                    result.push(FieldInfo {
                        name: Box::new(name.clone()),
                        pascal_case: capitalized,
                        ty: &field.ty,
                    });
                }
            }
        }
        Fields::Unnamed(unnamed) => {
            for (field, index) in unnamed.unnamed.iter().zip(0usize..) {
                if has_attr(&field.attrs, "delta_ignore").is_none() {
                    let name: syn::Index = Index::from(index);
                    let capitalized: syn::Ident = format_ident!("Field{}", index);
                    result.push(FieldInfo {
                        name: Box::new(name),
                        pascal_case: capitalized,
                        ty: &field.ty,
                    });
                }
            }
        }
        Fields::Unit => {}
    }
    result
}

fn map_field_types(fields: &syn::Fields, f: impl Fn(&syn::Type) -> syn::Type) -> syn::Fields {
    match fields {
        syn::Fields::Named(named) => syn::Fields::Named(syn::FieldsNamed {
            brace_token: named.brace_token,
            named: named
                .named
                .iter()
                .map(|field| {
                    if has_attr(&field.attrs, "delta_ignore").is_none() {
                        Some(syn::Field {
                            ty: f(&field.ty),
                            ..field.clone()
                        })
                    } else {
                        None
                    }
                })
                .flatten()
                .collect(),
        }),
        syn::Fields::Unnamed(unnamed) => syn::Fields::Unnamed(syn::FieldsUnnamed {
            paren_token: unnamed.paren_token,
            unnamed: unnamed
                .unnamed
                .iter()
                .map(|field| {
                    if has_attr(&field.attrs, "delta_ignore").is_none() {
                        Some(syn::Field {
                            ty: f(&field.ty),
                            ..field.clone()
                        })
                    } else {
                        None
                    }
                })
                .flatten()
                .collect(),
        }),
        syn::Fields::Unit => syn::Fields::Unit,
    }
}

fn _create_data_struct(fields: &syn::Fields) -> syn::DataStruct {
    syn::DataStruct {
        struct_token: syn::token::Struct {
            span: Span::call_site(),
        },
        fields: fields.clone(),
        semi_token: None,
    }
}

/// A mirror struct copies the exact fields of another structure (unless it
/// had unnamed fields, and `use_unnamed_fields` is false, in which case all
/// the unnamed fields will be given names of field0, field1, etc.). During
/// the copy, however, the types are substituted by an associated type of the
/// `Delta` trait.
fn create_mirror_struct(
    visibility: &syn::Visibility,
    type_name: &syn::Ident,
    suffix: &str,
    fields: &syn::Fields,
    use_unnamed_fields: bool,
) -> proc_macro2::TokenStream {
    define_struct_from_fields(
        visibility,
        &format_ident!("{}{}", type_name, suffix),
        #[allow(unused_variables)] // compiler doesn't see the use of ty
        &map_field_types(&fields, |ty: &syn::Type| -> syn::Type {
            let suffix_ident = format_ident!("{}", suffix);
            syn::parse2(quote!(<#ty as delta::Delta>::#suffix_ident))
                .expect(&format!("Failed to parse associated type for {}", suffix))
        }),
        use_unnamed_fields,
    )
}

fn define_enum_from_fields(
    visibility: &syn::Visibility,
    name: &syn::Ident,
    fields: &syn::Fields,
) -> proc_macro2::TokenStream {
    let change_innards: Vec<proc_macro2::TokenStream> = field_names_and_types(fields)
        .iter()
        .map(
            |FieldInfo {
                 name: _,
                 pascal_case,
                 ty,
             }| {
                let ch = change_type(ty);
                quote!(#pascal_case(#ch))
            },
        )
        .collect();
    definition(visibility, quote!(enum), name, false, change_innards)
}

fn define_struct_from_fields(
    visibility: &syn::Visibility,
    name: &syn::Ident,
    fields: &syn::Fields,
    use_unnamed_fields: bool,
) -> proc_macro2::TokenStream {
    let mut struct_fields = Vec::<proc_macro2::TokenStream>::new();
    match &fields {
        Fields::Named(named) => {
            for field in named.named.iter() {
                if has_attr(&field.attrs, "delta_ignore").is_none() {
                    let field_name: &syn::Ident = field.ident.as_ref().unwrap();
                    let ty = &field.ty;
                    struct_fields.push(quote!(#field_name: #ty));
                }
            }
        }
        Fields::Unnamed(unnamed) => {
            for (field, index) in unnamed.unnamed.iter().zip(0usize..) {
                if has_attr(&field.attrs, "delta_ignore").is_none() {
                    let ty = &field.ty;
                    if use_unnamed_fields {
                        struct_fields.push(quote!(#ty));
                    } else {
                        let field_name: syn::Ident =
                            Ident::new(&format!("field{}", index), Span::call_site());
                        struct_fields.push(quote!(#field_name: #ty));
                    }
                }
            }
        }
        Fields::Unit => {}
    }
    definition(
        visibility,
        quote!(struct),
        name,
        use_unnamed_fields,
        struct_fields,
    )
}

fn definition(
    visibility: &syn::Visibility,
    keyword: proc_macro2::TokenStream,
    name: &syn::Ident,
    use_unnamed_fields: bool,
    body: Vec<proc_macro2::TokenStream>,
) -> proc_macro2::TokenStream {
    if body.is_empty() {
        quote! {
            // #[derive(PartialEq, Debug, serde::Serialize, serde::Deserialize)]
            #[derive(PartialEq, Debug)]
            #visibility #keyword #name;
        }
    } else if body.len() == 1 {
        let singleton = &body[0];
        quote! {
            // #[derive(PartialEq, Debug, serde::Serialize, serde::Deserialize)]
            #[derive(PartialEq, Debug)]
            #visibility #keyword #name(#singleton);
        }
    } else if use_unnamed_fields {
        quote! {
            // #[derive(PartialEq, Debug, serde::Serialize, serde::Deserialize)]
            #[derive(PartialEq, Debug)]
            #visibility #keyword #name(#(#body),*);
        }
    } else {
        quote! {
            // #[derive(PartialEq, Debug, serde::Serialize, serde::Deserialize)]
            #[derive(PartialEq, Debug)]
            #visibility #keyword #name {
                #(#body),*
            }
        }
    }
}

fn define_delta_impl(
    name: &syn::Ident,
    describe_type: &proc_macro2::TokenStream,
    describe_body: &proc_macro2::TokenStream,
    change_type: &proc_macro2::TokenStream,
    change_body: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        impl delta::Delta for #name {
            type Desc = #describe_type;

            fn describe(&self) -> Self::Desc {
                #describe_body
            }

            type Change = #change_type;

            fn delta(&self, other: &Self) -> delta::Changed<Self::Change> {
                #change_body
            }
        }
    }
}

fn desc_type(ty: &syn::Type) -> proc_macro2::TokenStream {
    quote!(<#ty as delta::Delta>::Desc)
}

fn change_type(ty: &syn::Type) -> proc_macro2::TokenStream {
    quote!(<#ty as delta::Delta>::Change)
}
