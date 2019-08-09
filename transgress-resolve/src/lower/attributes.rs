//! Attribute lowering.

use super::ModuleCtx;
use lazy_static::lazy_static;
use syn;
use tracing::warn;
use transgress_api::{
    attributes::{Attribute, Deprecation, Meta, MetaInner, Metadata, Span, Visibility},
    paths::Path,
    tokens::Tokens,
};

lazy_static! {
    // the string used by `syn` for converting doc comments to attributes
    static ref DOCS: Path = Path::fake("doc");
    static ref MUST_USE: Path = Path::fake("must_use");
    static ref DEPRECATED: Path = Path::fake("deprecated");
    static ref SINCE: Path = Path::fake("since");
    static ref NOTE: Path = Path::fake("note");
}

/// Lower a bunch of syn data structures to the generic `ItemMetadata`.
pub fn lower_metadata(
    module: &ModuleCtx,
    visibility: &syn::Visibility,
    attributes: &[syn::Attribute],
    span: proc_macro2::Span,
) -> Metadata {
    let visibility = match visibility {
        syn::Visibility::Public(_) => Visibility::Pub,
        syn::Visibility::Inherited => module.visibility,
        _ => Visibility::NonPub,
    };
    let mut docs = None;
    let mut must_use = None;
    let mut deprecated = None;
    let mut extra_attributes = vec![];

    for syn_attr in attributes {
        let attr = parse_attribute(syn_attr);
        if attr.path() == &*DOCS {
            docs = Some(
                if let Attribute::Meta(Meta::Assign { literal, .. }) = attr {
                    extract_string(&literal)
                } else {
                    warn!("malformed doc attribute");
                    "".into()
                },
            );
        } else if attr.path() == &*MUST_USE {
            must_use = Some(
                if let Attribute::Meta(Meta::Assign { literal, .. }) = attr {
                    extract_string(&literal)
                } else {
                    warn!("malformed attribute");
                    "".into()
                },
            );
        } else if attr.path() == &*DEPRECATED {
            deprecated = Some(if let Attribute::Meta(Meta::Call { args, .. }) = &attr {
                let mut since = None;
                let mut note = None;
                for arg in args {
                    if let MetaInner::Meta(Meta::Assign { path, literal }) = arg {
                        if path == &*SINCE {
                            since = Some(extract_string(&literal));
                        } else if path == &*NOTE {
                            note = Some(extract_string(&literal));
                        } else {
                            warn!("unexpected #[deprecated] arg: {:?}", path);
                        }
                    } else {
                        warn!("malformed #[deprecated]: {:?}", attr);
                    }
                }
                Deprecation { since, note }
            } else {
                warn!("malformed #[deprecated]: {:?}", attr);
                Deprecation {
                    since: None,
                    note: None,
                }
            })
        } else {
            extra_attributes.push(attr);
        }
    }

    let span = Span::from_syn(module.source_file.clone(), span);

    Metadata {
        visibility,
        docs,
        must_use,
        deprecated,
        extra_attributes,
        span,
    }
}

fn parse_attribute(attribute: &syn::Attribute) -> Attribute {
    if let Ok(meta) = attribute.parse_meta() {
        Attribute::Meta(lower_meta(&meta))
    } else {
        Attribute::Other {
            path: (&attribute.path).into(),
            input: Tokens::from(&attribute.tts),
        }
    }
}

/// Lower a syn Meta to our Meta.
fn lower_meta(meta: &syn::Meta) -> Meta {
    // TODO: update this when syn merges the paths breaking change
    match meta {
        syn::Meta::Word(ident) => Meta::Path(Path::ident(ident.into())),
        syn::Meta::NameValue(syn::MetaNameValue { ident, lit, .. }) => Meta::Assign {
            path: Path::ident(ident.into()),
            literal: Tokens::from(lit),
        },
        syn::Meta::List(syn::MetaList { ident, nested, .. }) => Meta::Call {
            path: Path::ident(ident.into()),
            args: nested
                .iter()
                .map(|arg| match arg {
                    syn::NestedMeta::Meta(meta) => MetaInner::Meta(lower_meta(meta)),
                    syn::NestedMeta::Literal(lit) => MetaInner::Literal(Tokens::from(lit)),
                })
                .collect(),
        },
    }
}

/// TODO replace this w/ proper PM2 shim
fn extract_string(lit: &Tokens) -> String {
    if let Ok(lit) = syn::parse2::<syn::LitStr>(lit.get_tokens()) {
        lit.value()
    } else {
        warn!("failed to extract string from {:?}", lit);
        lit.get_tokens().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use std::path::PathBuf;
    use syn::{parse_quote, spanned::Spanned};
    use transgress_api::attributes::Deprecation;

    #[test]
    fn metadata_lowering() {
        let all = lower_metadata(
            &ModuleCtx {
                source_file: PathBuf::from("fake_file.rs"),
                visibility: Visibility::Pub,
            },
            &parse_quote!(pub),
            &[
                parse_quote!(
                    /// this is an item that exists
                ),
                parse_quote!(#[must_use = "use me"]),
                parse_quote!(#[deprecated(since = "0.2.0", note = "don't use me")]),
                parse_quote!(#[other_attribute]),
                parse_quote!(#[other_attribute_meta(thing = "baz")]),
                parse_quote!(#[other_attribute_weird 2 + 2 / 3 - 4]),
            ],
            quote!(_).span(),
        );
        assert_match!(all, Metadata {
            visibility: Visibility::Pub,
            docs: Some(docs),
            must_use: Some(must_use),
            deprecated: Some(Deprecation { note: Some(note), since: Some(since) }),
            extra_attributes,
            ..
        } => {
            assert_eq!(docs, " this is an item that exists");
            assert_eq!(must_use, "use me");
            assert_eq!(since, "0.2.0");
            assert_eq!(note, "don't use me");

            assert_match!(extra_attributes[0], Attribute::Meta(Meta::Path(path)) => {
                assert_eq!(path, &Path::fake("other_attribute"))
            });

            assert_match!(extra_attributes[1], Attribute::Meta(Meta::Call {
                path, args
            }) => {
                assert_eq!(path, &Path::fake("other_attribute_meta"));
                assert_match!(args[0], MetaInner::Meta(Meta::Assign { path, literal }) => {
                    assert_eq!(path, &Path::fake("thing"));
                    assert_eq!(literal.get_tokens().to_string(), quote!("baz").to_string());
                });
            });

            assert_match!(extra_attributes[2], Attribute::Other{
                path, input
            } => {
                assert_eq!(path, &Path::fake("other_attribute_weird"));
                assert_eq!(input.to_string(), quote!(2 + 2 / 3 - 4).to_string());
            });
        });

        let inherits_public = lower_metadata(
            &ModuleCtx {
                source_file: PathBuf::from("fake_file.rs"),
                visibility: Visibility::Pub,
            },
            &parse_quote!(),
            &[],
            quote!(_).span(),
        );
        assert_eq!(inherits_public.visibility, Visibility::Pub);
        ;

        let inherits_private = lower_metadata(
            &ModuleCtx {
                source_file: PathBuf::from("fake_file.rs"),
                visibility: Visibility::NonPub,
            },
            &parse_quote!(),
            &[],
            quote!(_).span(),
        );
        assert_eq!(inherits_private.visibility, Visibility::NonPub);
    }
}