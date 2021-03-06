use super::LowerError;
use crate::lower::attributes::extract_symbol_metadata;
use crate::lower::generics::lower_lifetime;
use crate::lower::types::lower_return_type;
use crate::lower::{
    attributes::{extract_type_metadata, lower_metadata},
    generics::lower_generics,
    types::lower_type,
};
use crate::walker::LocationMetadata;
use syn::spanned::Spanned;
use tendon_api::items::{FunctionArg, FunctionItem, Receiver, Signature};
use tendon_api::{
    items::{Abi, EnumItem, EnumVariant, InherentImpl, StructField, StructItem, StructKind},
    paths::Ident,
    tokens::Tokens,
};

/// Lower a struct.
pub(crate) fn lower_struct(
    loc: &LocationMetadata,
    struct_: &syn::ItemStruct,
) -> Result<StructItem, LowerError> {
    let mut metadata =
        super::attributes::lower_metadata(loc, &struct_.vis, &struct_.attrs, struct_.span())?;

    let type_metadata = extract_type_metadata(&mut metadata)?;

    let generics = lower_generics(&struct_.generics)?;

    let kind = match struct_.fields {
        syn::Fields::Named(..) => StructKind::Named,
        syn::Fields::Unnamed(..) => StructKind::Tuple,
        syn::Fields::Unit => StructKind::Unit,
    };

    let fields = lower_fields(loc, &struct_.fields)?;

    let inherent_impl = InherentImpl {};

    let name = Ident::from(&struct_.ident);

    Ok(StructItem {
        fields,
        generics,
        inherent_impl,
        kind,
        metadata,
        name,
        type_metadata,
    })
}

/// Lower an enum.
pub(crate) fn lower_enum(
    loc: &LocationMetadata,
    enum_: &syn::ItemEnum,
) -> Result<EnumItem, LowerError> {
    let mut metadata =
        super::attributes::lower_metadata(loc, &enum_.vis, &enum_.attrs, enum_.span())?;
    let type_metadata = extract_type_metadata(&mut metadata)?;

    let generics = lower_generics(&enum_.generics)?;

    let variants = enum_
        .variants
        .iter()
        .map(|variant| {
            // Note: we copy the parent's visibility:
            let metadata =
                super::attributes::lower_metadata(loc, &enum_.vis, &variant.attrs, variant.span())?;

            let kind = match variant.fields {
                syn::Fields::Named(..) => StructKind::Named,
                syn::Fields::Unnamed(..) => StructKind::Tuple,
                syn::Fields::Unit => StructKind::Unit,
            };

            let fields = lower_fields(loc, &variant.fields)?;

            let name = Ident::from(&variant.ident);
            Ok(EnumVariant {
                metadata,
                kind,
                fields,
                name,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let inherent_impl = InherentImpl {};

    let name = Ident::from(&enum_.ident);

    Ok(EnumItem {
        generics,
        inherent_impl,
        metadata,
        name,
        type_metadata,
        variants,
    })
}

fn lower_fields(
    loc: &LocationMetadata,
    fields: &syn::Fields,
) -> Result<Vec<StructField>, LowerError> {
    fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let metadata = lower_metadata(loc, &field.vis, &field.attrs, field.span())?;
            let name = field
                .ident
                .as_ref()
                .map(|ident| Ident::from(ident))
                .unwrap_or_else(|| Ident::from(&format!("{}", i)[..]));
            let type_ = lower_type(&field.ty)?;

            Ok(StructField {
                metadata,
                name,
                type_,
            })
        })
        .collect()
}

/// Lower a function.
/// Annoyingly, the data for this is stored in different places for functions / methods
/// so you just have to pass in a bunch of junk lol.
pub fn lower_signature(sig: &syn::Signature) -> Result<Signature, LowerError> {
    let mut receiver = Receiver::None;
    let variadic = sig.variadic.is_some();

    // this is hairy. idgaf
    let args = sig
        .inputs
        .iter()
        .enumerate()
        // walk through arguments, pulling out receiver if present
        .filter(|(i, arg)| match arg {
            syn::FnArg::Receiver(rec) => {
                if let Some((_, lifetime)) = &rec.reference {
                    let lifetime = lifetime.as_ref().map(lower_lifetime);
                    let mut_ = rec.mutability.is_some();
                    receiver = Receiver::RefSelf { lifetime, mut_ };
                } else {
                    receiver = Receiver::ConsumeSelf
                }
                false
            }
            _ => {
                if variadic {
                    // skip last arg for variadics, can't be parsed
                    *i < sig.inputs.len() - 1
                } else {
                    true
                }
            }
        })
        .map(|(_, arg)| match arg {
            syn::FnArg::Typed(typed) => {
                let type_ = lower_type(&typed.ty)?;
                let name = if let syn::Pat::Ident(pat_ident) = &*typed.pat {
                    Ident::from(&pat_ident.ident)
                } else {
                    Ident::from("_")
                };
                Ok(FunctionArg { type_, name })
            }
            _ => Err(LowerError::MalformedFunctionArg(Tokens::from(arg))),
        })
        .collect::<Result<Vec<FunctionArg>, LowerError>>()?;

    let ret = lower_return_type(&sig.output)?;
    let is_unsafe = sig.unsafety.is_some();
    let is_async = sig.asyncness.is_some();
    let is_const = sig.constness.is_some();
    let abi = sig
        .abi
        .as_ref()
        .map(|abi| {
            if let Some(name) = &abi.name {
                // if there is an abi string:
                match &name.value()[..] {
                    "Rust" => Abi::Rust,
                    "C" => Abi::C,
                    other => Abi::Other(other.to_string()),
                }
            } else {
                // only an extern token
                Abi::C
            }
        })
        .unwrap_or(
            // no extern at all
            Abi::Rust,
        );
    let generics = lower_generics(&sig.generics)?;

    Ok(Signature {
        abi,
        args,
        generics,
        is_async,
        is_const,
        is_unsafe,
        receiver,
        ret,
        variadic,
    })
}

/// Lower a function item.
pub(crate) fn lower_function_item(
    loc: &LocationMetadata,
    item: &syn::ItemFn,
) -> Result<FunctionItem, LowerError> {
    let mut metadata = lower_metadata(loc, &item.vis, &item.attrs, item.span())?;
    let symbol_metadata = extract_symbol_metadata(&mut metadata)?;
    let name = Ident::from(&item.sig.ident);
    let signature = lower_signature(&item.sig)?;
    Ok(FunctionItem {
        metadata,
        symbol_metadata,
        name,
        signature,
    })
}

/*
/// Lower a method.
pub fn lower_impl_method(
    loc: &LocationMetadata,
    item: &syn::ImplItemMethod,
) -> Result<Signature, LowerError> {
    Ok(lower_signature(
        loc,
        &item.sig.sig,
        &item.attrs,
        &item.vis,
        &item.sig.ident,
        &item.sig.abi,
        &item.sig.constness,
        &item.sig.asyncness,
        &item.sig.unsafety,
        item.span(),
    )?)
}

/// Lower a method.
pub fn lower_trait_method(
    loc: &LocationMetadata,
    item: &syn::TraitItemMethod,
    trait_vis: &syn::Visibility,
) -> Result<Signature, LowerError> {
    Ok(lower_signature(
        loc,
        &item.sig.sig,
        &item.attrs,
        trait_vis,
        &item.sig.ident,
        &item.sig.abi,
        &item.sig.constness,
        &item.sig.asyncness,
        &item.sig.unsafety,
        item.span(),
    )?)
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::TEST_LOCATION_METADATA;
    use tendon_api::attributes::{Repr, Visibility};
    use tendon_api::paths::Path;
    use tendon_api::types::{PathType, Type};

    #[test]
    fn struct_lowering() {
        spoor::init();
        let struct_: syn::ItemStruct = syn::parse_quote! {
            /// This is an example struct.
            #[derive(Clone)]
            #[repr(C)]
            pub struct Thing<'a, T> where T: Clone + 'a {
                /// This is a reference to a different thing.
                pub reference: &'a T,
                others: Vec<&'a T>,
                count: i32,
                path: &'a std::path::Path,
            }
        };
        let struct_ = lower_struct(&TEST_LOCATION_METADATA, &struct_).unwrap();

        assert_eq!(struct_.name, Ident::from("Thing"));

        assert_eq!(struct_.metadata.visibility, Visibility::Pub);
        assert_eq!(struct_.type_metadata.repr, Repr::C);
        assert_eq!(struct_.type_metadata.derives[0].path, Path::fake("Clone"));
        assert_eq!(struct_.kind, StructKind::Named);
        assert_eq!(struct_.fields.len(), 4);
        assert_eq!(struct_.fields[0].name, Ident::from("reference"));
        assert_eq!(struct_.fields[1].name, Ident::from("others"));
        assert_eq!(struct_.fields[2].name, Ident::from("count"));
        assert_eq!(struct_.fields[3].name, Ident::from("path"));

        assert_match!(struct_.fields[2].type_, Type::Path(PathType { path, params }) => {
            assert_eq!(path, &Path::fake("i32"));
            assert!(params.is_empty());
        });

        assert_eq!(struct_.fields[0].metadata.visibility, Visibility::Pub);
        assert_eq!(struct_.fields[1].metadata.visibility, Visibility::NonPub);
        assert_eq!(struct_.fields[2].metadata.visibility, Visibility::NonPub);
        assert_eq!(struct_.fields[3].metadata.visibility, Visibility::NonPub);
        assert_eq!(
            struct_.fields[0].metadata.docs,
            Some(" This is a reference to a different thing.".into())
        );
    }

    #[test]
    fn enum_lowering() {
        spoor::init();
        let enum_: syn::ItemEnum = syn::parse_quote! {
            #[repr(C, i8)]
            pub enum Thing2 {
                /// enum variant
                #[attribute = "banana"]
                Variant1,
                Variant2(i32),
                Variant3 { val: i32 }
            }
        };
        let enum_ = lower_enum(&TEST_LOCATION_METADATA, &enum_).unwrap();

        assert_eq!(enum_.name, Ident::from("Thing2"));
        assert_eq!(
            enum_.type_metadata.repr,
            Repr::IntOuterTag(Ident::from("i8"))
        );

        assert_eq!(enum_.variants.len(), 3);
        assert_eq!(enum_.variants[0].name, Ident::from("Variant1"));
        assert_eq!(enum_.variants[1].name, Ident::from("Variant2"));
        assert_eq!(enum_.variants[2].name, Ident::from("Variant3"));
        assert_eq!(enum_.variants[0].kind, StructKind::Unit);
        assert_eq!(enum_.variants[1].kind, StructKind::Tuple);
        assert_eq!(enum_.variants[2].kind, StructKind::Named);

        assert_eq!(
            enum_.variants[0].metadata.docs,
            Some(" enum variant".into())
        );
        assert_eq!(enum_.variants[0].metadata.extra_attributes.len(), 1);
        assert_eq!(
            enum_.variants[0].metadata.extra_attributes[0].path(),
            &Path::fake("attribute")
        );
        assert_eq!(
            enum_.variants[0].metadata.extra_attributes[0].get_assigned_string(),
            Some("banana".into())
        );

        assert_eq!(enum_.variants[0].fields.len(), 0);
        assert_eq!(enum_.variants[1].fields.len(), 1);
        assert_eq!(enum_.variants[2].fields.len(), 1);

        assert_eq!(enum_.variants[1].fields[0].name, Ident::from("0"));
        assert_eq!(enum_.variants[2].fields[0].name, Ident::from("val"));
    }

    #[test]
    fn function_lowering() {
        spoor::init();
        let function_ = syn::parse_quote! {
            #[no_mangle]
            #[export_name = "orange"]
            #[link_section = ".banana"]
            pub const async unsafe extern "system" fn f<T: Copy>(t: &T, rest: ...) -> i32 {}
        };
        let function_ = lower_function_item(&TEST_LOCATION_METADATA, &function_);
        let function_ = function_.unwrap();
        assert!(function_.signature.is_const);
        assert!(function_.signature.is_unsafe);
        assert!(function_.signature.is_async);
        assert_match!(function_.signature.abi, Abi::Other(other) => {
            assert_eq!(other, "system");
        });
        assert!(function_.signature.variadic);
        assert!(function_.symbol_metadata.no_mangle);
        assert_eq!(function_.symbol_metadata.export_name, Some("orange".into()));
        assert_eq!(
            function_.symbol_metadata.link_section,
            Some(".banana".into())
        );
        assert!(!function_.signature.generics.is_empty());
        assert!(!function_.signature.args.is_empty());
        assert!(!function_.signature.ret.is_void());
        assert_eq!(function_.name, Ident::from("f"));

        let function_ = syn::parse_quote! {
            fn g() {}
        };
        let function_ = lower_function_item(&TEST_LOCATION_METADATA, &function_).unwrap();
        assert!(!function_.signature.is_const);
        assert!(!function_.signature.is_unsafe);
        assert!(!function_.signature.is_async);
        assert_match!(function_.signature.abi, Abi::Rust);
        assert!(!function_.signature.variadic);
        assert!(!function_.symbol_metadata.no_mangle);
        assert_eq!(function_.symbol_metadata.export_name, None);
        assert_eq!(function_.symbol_metadata.link_section, None);
        assert!(function_.signature.generics.is_empty());
        assert!(function_.signature.args.is_empty());
        assert!(function_.signature.ret.is_void());
        assert_eq!(function_.name, Ident::from("g"));
    }
}
