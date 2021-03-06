//! Walk through a crate, parsing with syn and then a `resolver::Db`. This is the core name resolution
//! algorithm.
//!
//! This code is serial but multiple crates can be read into the same Db at once.
//!
//! Currently only Rust 2018 name resolution has been implemented, and even then, it's somewhat
//! sketchy. Rust's name resolution and macro expansion rules constantly change and aren't fully
//! documented everywhere.
//!
//! We permit resolution to fail, since we maintain the invariant that all code fed to `tendon` has
//! passed `cargo check`. Therefore, stuff we can't identify, we just ignore.
//!
//! We do require, however, that all of a crates dependencies be fully resolved before we attempt
//! to resolve that crate. Even if the crate could not be read (its path was wrong somehow, it's
//! Rust 2015 which we don't yet handle)
//!
//! TODO: tell the user what was missed, and give them workarounds somehow.
//!
//! TODO: we need to generate tests that ensure our bindings actually point to the correct places...
//! somehow.
//!
//! We roughly follow the pseudocode from RFC 1560. There's no guarantee that's actually correct
//! though.
//!
//! ```no_build
//! // Assumes parsing is already done, but the two things could be done in the same
//! // pass.
//! fn parse_expand_and_resolve() {
//!     loop until fixed point {
//!         process_names()
//!         loop until fixed point {
//!             process_work_list()
//!         }
//!         expand_macros()
//!     }
//!
//!     for item in work_list {
//!         report_error()
//!     } else {
//!         success!()
//!     }
//! }
//!
//! fn process_names() {
//!     // 'module' includes `mod`s, top level of the crate, function bodies
//!     for each unseen item in any module {
//!         if item is a definition {
//!             // struct, trait, type, local variable def, etc.
//!             bindings.insert(item.name, module, item)
//!             populate_back_links(module, item)
//!         } else {
//!             try_to_resolve_import(module, item)
//!         }
//!         record_macro_uses()
//!     }
//! }
//! fn try_to_resolve_import(module, item) {
//!     if item is an explicit use {
//!         // item is use a::b::c as d;
//!         match try_to_resolve(item) {
//!             Ok(r) => {
//!                 add(bindings.insert(d, module, r, Priority::Explicit))
//!                 populate_back_links(module, item)
//!             }
//!             Err() => work_list.push(module, item)
//!         }
//!     } else if item is a glob {
//!         // use a::b::*;
//!         match try_to_resolve(a::b) {
//!             Ok(n) => {
//!                 for binding in n {
//!                     bindings.insert_if_no_higher_priority_binding(binding.name, module, binding, Priority::Glob)
//!                     populate_back_links(module, binding)
//!                 }
//!                 add_back_link(n to module)
//!                 work_list.remove()
//!             }
//!             Err(_) => work_list.push(module, item)
//!         }
//!     }
//! }
//! fn process_work_list() {
//!     for each (module, item) in work_list {
//!         work_list.remove()
//!         try_to_resolve_import(module, item)
//!     }
//! }
//! ```
//!
//! references:
//! https://rust-lang.github.io/rustc-guide/macro-expansion.html
//! https://rust-lang.github.io/rustc-guide/name-resolution.html
//! https://doc.rust-lang.org/nightly/edition-guide/rust-2018/macros/macro-changes.html
//! https://github.com/rust-lang/rfcs/blob/master/text/0453-macro-reform.md
//! https://github.com/rust-lang/rfcs/blob/master/text/1560-name-resolution.md
//! https://github.com/rust-lang/rfcs/blob/master/text/2126-path-clarity.md
//! https://internals.rust-lang.org/t/relative-paths-and-rust-2018-use-statements/7875
//! https://internals.rust-lang.org/t/up-to-date-documentation-on-macro-resolution-order/11877/5

use hashbrown::hash_map::Entry as HEntry;
use lazy_static::lazy_static;
use std::fs::File;
use std::io::Read;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tendon_api::attributes::{Metadata, Span, Visibility};
use tendon_api::crates::CrateData;
use tendon_api::database::{Crate, Db, NamespaceLookup};
use tendon_api::identities::{CrateId, Identity};
use tendon_api::paths::Ident;
use tendon_api::scopes::{NamespaceId, Priority, Scope};
use tendon_api::tokens::Tokens;
use tendon_api::{Map, Set};
use textual_scope::TextualScope;
use tracing::{error, trace, warn};

mod helpers;
mod textual_scope;

pub(crate) struct LocationMetadata<'a> {
    pub(crate) source_file: PathBuf,
    pub(crate) macro_invocation: Option<Arc<Span>>,
    pub(crate) crate_data: &'a CrateData,
    pub(crate) module_path: Identity,
}

lazy_static! {
    //static ref MACRO_USE: UnresolvedPath = Path::fake("macro_use");
    //static ref PATH: UnresolvedPath = Path::fake("path");
    static ref MACRO_RULES: Ident = "macro_rules".into();
    static ref CRATE: Ident = "crate".into();
    static ref SELF: Ident = "self".into();
    static ref SELF_TYPE: Ident = "Self".into();
    static ref SUPER: Ident = "super".into();
    pub(crate) static ref TEST_CRATE_DATA: CrateData = CrateData::fake(CrateId::new("fake_crate", "0.0.0"));
    pub(crate) static ref TEST_LOCATION_METADATA: LocationMetadata<'static> = LocationMetadata {
        source_file: "fake_file.rs".into(),
        macro_invocation: None,
        crate_data: &*TEST_CRATE_DATA,
        module_path: Identity {
            crate_: CrateId::new("fake_crate", "0.0.0"),
            path: vec![]
        }
    };
}

quick_error! {
    // could break this out into sub-errors...
    #[derive(Debug)]
    pub enum WalkError {
        Io(err: std::io::Error) {
            from()
            cause(err)
            display("io error during walking: {}", err)
        }
        Parse(err: syn::Error) {
            from()
            cause(err)
            display("parse error during walking: {}", err)
        }
        MalformedPathAttribute(tokens: String) {
            display("malformed `#[path]` attribute: {}", tokens)
        }
        Database(err: DatabaseError) {
            from()
            cause(err)
            display("database error during walking: {}", err)
        }
        Root {
            display("files at fs root??")
        }
        ModuleNotFound {
            display("couldn't find source file")
        }
        //NonPub {
        //    display("skipping non-pub item (will never be accessible)")
        //}
        //NotYetResolved {
        //    display("item is not yet resolved")
        //}
        //CannotResolve {
        //    display("item cannot resolve")
        //}
        //Impossible {
        //    display("some invariant was violated (but we're still going, dammit...)")
        //}
    }
}

/// A module we haven't yet succeeded in expanding.
struct UnexpandedModule<'a> {
    /// Where this module is (in the file system and the crate namespace).
    loc: LocationMetadata<'a>,

    /// Items that we have not yet succeeded in expanding, along with their spans and textual
    /// scopes.
    /// Ordering doesn't matter here -- it's tracked by the TextualScopes instead.
    unexpanded_items: Vec<(Span, TextualScope, UnexpandedItem)>,
}

/// An item we have not yet succeeded in expanding.
#[derive(Debug)]
enum UnexpandedItem {
    /// A macro invocation in item position,
    UnresolvedMacroInvocation(Tokens),

    /// Some item that contains a macro in type position.
    TypeMacro(Tokens),

    /// Something with an attribute macro applied.
    AttributeMacro(Tokens),

    /// Something with a derive macro applied.
    DeriveMacro(Tokens),
}

/// Parse a file into a syn::File.
fn parse_file(file: &FsPath) -> Result<syn::File, WalkError> {
    trace!("parsing `{}`", file.display());

    let mut file = File::open(file)?;
    let mut source = String::new();
    file.read_to_string(&mut source)?;

    Ok(syn::parse_file(&source)?)
}

fn skip(kind: &str, path: Identity) {
    trace!("skipping {} {:?}", kind, &path);
}

fn warn(cause: impl Into<WalkError>, span: &Span) {
    let cause = cause.into();
    //if let WalkError::Lower(LowerError::CfgdOut) = cause {
    //    // can just suppress this
    //    return;
    //}
    warn!("[{:?}]: suppressing error: {}", span, cause);
}

/// The first phase: walk files, parse, find imports, expand macros.
/// As items cease to have anything to do with macros they are dumped in the Db; after macro
/// expansion order no longer matters.
pub struct Walker<'a> {
    /// A Db containing resolved definitions for all dependencies.
    /// During this phase, we insert `ModuleItems` and `MacroItems` into this database.
    /// The only items that go in here are `#[macro_export]`-marked macros.
    db: &'a Db,

    /// The relevant CrateData.
    crate_data: &'a CrateData,

    /// The crate that's being built.
    crate_: Crate,

    /// Live metadata, will be discarded once we're finished with this crate.
    scopes_in_progress: Map<Identity, ScopeInProgress>,
}
impl<'a> Walker<'a> {
    /// Create a new walker. The crate_id must have been registered in the Db at creation time.
    pub fn new(db: &'a Db, crate_id: &CrateId) -> Walker<'a> {
        Walker {
            db,
            crate_data: db.crate_data(crate_id),
            crate_: Crate::new(crate_id.clone()),
            scopes_in_progress: Map::default(),
        }
    }

    /// Complete the walker, inserting the parsed crate into the database.
    pub fn complete(self) {
        let Walker { db, crate_, .. } = self;
        db.insert_crate(crate_);
    }

    /// Add an item to a crate. The item is added at `{containing_scope}::{item.metadata().name}`.
    /// The containing_scope must be in this crate.
    /// Also adds a Binding to the containing scope.
    pub fn add<I: NamespaceLookup>(
        &mut self,
        containing_scope: &Identity,
        item: I,
    ) -> Result<Identity, DatabaseError> {
        assert_eq!(
            containing_scope.crate_, self.crate_.id,
            "can't add item to a different crate!!"
        );

        let name = item.metadata().name.clone();
        let visibility = item.metadata().visibility.clone();

        let identity = containing_scope.clone_join(name.clone());
        let Identity { path, crate_ } = identity.clone();

        match I::get_namespace_mut(&mut self.crate_).0.entry(path) {
            HEntry::Vacant(vacant) => {
                vacant.insert(item);
            }
            HEntry::Occupied(occupied) => {
                error!(
                    "{:?} ({:?}) already defined!",
                    Identity::new(&crate_, occupied.key()),
                    I::namespace_id()
                );
                return Err(DatabaseError::ItemAlreadyPresent);
            }
        }

        self.add_binding::<I>(
            &containing_scope,
            name,
            identity.clone(),
            visibility,
            Priority::Explicit,
        )?;

        Ok(identity)
    }

    /// Add a binding to a scope. Doesn't have to target something in this crate.
    pub fn add_binding<I: NamespaceLookup>(
        &mut self,
        containing_scope: &Identity,
        name: Ident,
        target: Identity,
        visibility: Visibility,
        priority: Priority,
    ) -> Result<(), DatabaseError> {
        self.add_binding_by(
            containing_scope,
            I::namespace_id(),
            name,
            target,
            visibility,
            priority,
        )
    }

    #[inline(never)]
    pub fn add_binding_by(
        &mut self,
        containing_scope: &Identity,
        namespace_id: NamespaceId,
        name: Ident,
        target: Identity,
        visibility: Visibility,
        priority: Priority,
    ) -> Result<(), DatabaseError> {
        assert!(
            &containing_scope.crate_ == &self.crate_.id,
            "can't add a binding to another crate!"
        );

        let scope = self
            .crate_
            .get_mut::<Scope>(containing_scope)
            .ok_or(DatabaseError::NoSuchScope)?;

        // TODO allow replacing bindings somehow?
        scope
            .insert_by(namespace_id, name, target, visibility, priority)
            .map_err(|_| DatabaseError::BindingAlreadyPresent)
    }

    /// Add a binding to the prelude.
    pub fn add_prelude_binding_by(
        &mut self,
        namespace_id: NamespaceId,
        name: Ident,
        target: Identity,
    ) -> Result<(), DatabaseError> {
        let visibility = Visibility::InScope(Identity::root(&self.crate_.id));
        self.crate_
            .prelude
            .insert_by(namespace_id, name, target, visibility, Priority::Explicit)
            .map_err(|_| DatabaseError::BindingAlreadyPresent)
    }

    /// Add the root scope.
    pub fn add_root_scope(&mut self, metadata: Metadata) -> Result<Identity, DatabaseError> {
        assert!(&metadata.name[..] == "{root}");
        let item = Scope::new(metadata, true);

        match self.crate_.scopes.0.entry(vec![]) {
            HEntry::Vacant(vacant) => {
                vacant.insert(item);
                Ok(Identity::root(&self.crate_.id))
            }
            HEntry::Occupied(_) => {
                error!("{:?} root already defined!", &self.crate_.id);
                Err(DatabaseError::ItemAlreadyPresent)
            }
        }
    }
}

quick_error::quick_error! {
    #[derive(Debug, Clone, Copy)]
    pub enum DatabaseError {
        ItemAlreadyPresent {
            display("item is already present")
        }
        BindingAlreadyPresent {
            display("binding is already present")
        }
        NoSuchScope {
            display("no such scope")
        }
    }
}

/// Metadata for scopes in the crate that have not completed resolution.
/// Discarded once we've finished parsing the current crate.
struct ScopeInProgress {
    /// Back links: a map from scopes to glob imports.
    /// When we add a binding here, we have to go to all of those and add it there too (with the
    /// accompanying visibility.)
    /// If `crate::a` has `use crate::b::*`, then `crate::a` imports everything from `crate::b`,
    /// but also gets a back link. as more items are resolved in B, they get added to A.
    /// Only relevant in the current crate; if we import from a dependency, we know it's frozen.
    back_links: Vec<(Identity, Visibility)>,

    /// Imports that are explicit but may not be resolved yet.
    /// If an explicit import exists, we never add any glob names that match it.
    reserved_explicit_imports: Set<Ident>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tendon_api::attributes::TypeMetadata;
    use tendon_api::identities::TEST_CRATE_A;
    use tendon_api::items::{EnumItem, GenericParams, TypeItem};

    #[test]
    fn crate_building() {
        let db = Db::fake_db();
        let test_crate_a = (*TEST_CRATE_A).clone();

        let mut walker = Walker::new(&db, &test_crate_a);

        let root = walker.add_root_scope(Metadata::fake("{root}")).unwrap();

        assert!(walker.crate_.get::<Scope>(&root).is_some());

        let mod_a = walker
            .add(&root, Scope::new(Metadata::fake("a"), true))
            .unwrap();

        assert!(walker.crate_.get::<Scope>(&mod_a).is_some());
        assert!(
            &walker
                .crate_
                .get_binding::<Scope>(&root, &"a".into())
                .unwrap()
                .identity
                == &mod_a
        );

        let mod_a_b = walker
            .add(&mod_a, Scope::new(Metadata::fake("a"), true))
            .unwrap();

        let type_a_b_c = walker
            .add(
                &mod_a_b,
                TypeItem::Enum(EnumItem {
                    metadata: Metadata::fake("C"),
                    type_metadata: TypeMetadata::default(),
                    generic_params: GenericParams::default(),
                    variants: vec![],
                }),
            )
            .unwrap();

        walker
            .add_binding::<TypeItem>(
                &root,
                "CRenamed".into(),
                type_a_b_c.clone(),
                Visibility::InScope(root.clone()),
                Priority::Explicit,
            )
            .unwrap();

        let in_root = walker
            .crate_
            .get_binding::<TypeItem>(&root, &"CRenamed".into())
            .unwrap();
        let in_a_b = walker
            .crate_
            .get_binding::<TypeItem>(&mod_a_b, &"C".into())
            .unwrap();

        assert_eq!(in_root.identity, in_a_b.identity);
        assert_eq!(in_root.visibility, Visibility::InScope(root.clone()));
        assert_eq!(in_a_b.visibility, Visibility::Pub);
    }
}

/*
/// Walk a whole crate, expanding macros, storing all resulting data in the central db.
/// This operates serially but multiple threads can operate on the same db in parallel.
fn walk_crate(crate_data: &CrateData, db: &Db) -> Result<(), WalkError> {
    trace!("walking {:?}", crate_data.crate_);

    let mut crate_data = crate_data.clone();
    let mut macro_prelude: Map<Ident, DeclarativeMacroItem> = Map::default();

    // Load the root entry
    let file = parse_file(&crate_data.entry)?;

    // Special handling: `extern crate`
    for item in &file.items {
        if let syn::Item::ExternCrate(extern_crate) = item {
            let result = (|| -> Result<(), WalkError> {
                let ident = Ident::from(&extern_crate.ident);
                let crate_ = ctx
                    .crate_data
                    .deps
                    .get(&ident)
                    .ok_or_else(|| WalkError::ExternCrateNotFound(ident.clone()))?;

                let macro_use = extern_crate.attrs.iter()
                    .any(|attr| attr.path.is_ident("macro_use"));

                if macro_use {
                    for item in db.macros.

                }

                if metadata.extract_attribute(&*MACRO_USE).is_some() {
                    // this miiiight have weird ordering consequences... whatever
                    ctx.unexpanded
                        .insert(UnexpandedItem::MacroUse(span.clone(), crate_.clone()));
                }

            })();

            /*
            fn handle_root_extern_crate(loc: &LocationMetadata, extern_crate: &syn::ItemExternCrate) {
                let mut metadata = lower_metadata(
                    loc,
                    &extern_crate.vis,
                    &extern_crate.attrs,
                    extern_crate.span(),
                )?;



                if let Some((_, name)) = &extern_crate.rename {
                    // add rename to crate namespace
                    // note that this effect *only* occurs at the crate root: otherwise `extern crate`
                    // just behaves like a `use`.
                    crate_data.deps.insert(Ident::from(&name), crate_.clone());
                }
            }
            */


        }
    }


    // special handling at crate root: extern crate
    let root_path = Identity::root(crate_data.crate_.clone());
    let source_root = crate_data.entry.parent().unwrap().to_path_buf();

    let mut phase = WalkParseExpandPhase {
        db: Db,
        unexpanded_modules: Map::default(),
        macro_prelude: Map::default()
    };


    // TODO extern crate extra effects at crate root:
    // - global alias
    // - #[macro_use] injects into all modules

    /*
    let root_module = ParsedModule {
        loc: LocationMetadata {
            source_file: &crate_data.entry.clone(),
            macro_invocation: None
        },
        items: vec![],
        scope: Default::default()
    };

    // prep modules
    let mut imports = ModuleScope::new();

    // the root context
    let mut ctx = WalkModuleCtx {
        source_file: crate_data.entry.clone(),
        module: root_path.clone(),
        db,
        scope: &mut imports,
        unexpanded: UnexpandedCursor::new(&mut unexpanded),
        crate_unexpanded_modules: &crate_unexpanded_modules,
        crate_data: &crate_data.clone(),
        macro_invocation: None,
    };

    // special case: walk the crate root looking for extern crates.
    // they behave special here for legacy reasons.

    // patch in data w/ modified extern crate names
    ctx.crate_data = crate_data;

    // get metadata for root crate entry
    let metadata = lower_metadata(&mut ctx, &syn::parse_quote!(pub), &file.attrs, file.span())?;
    let module = ModuleItem {
        name: Ident::from(root_path.crate_.name.to_string()),
        metadata,
    };

    // store results for root crate entry
    db.modules.insert(root_path.clone(), module)?;
    crate_unexpanded_modules.insert(root_path.clone(), unexpanded);

    //
    walk_items(&mut ctx, &file.items)?;

    let mut local_sequential_macros = Map::default();

    expand_module(
        db,
        &crate_unexpanded_modules,
        root_path.clone(),
        crate_data,
        &mut local_sequential_macros,
    )?;
    */

    Ok(())
}

/// Walk a list of items. They are stored in the phase data structures, either in `db` or
/// `unexpanded_modules`.
///
/// Note that this function cannot return an error! errors for individual items are currently just traced.
/// TODO: we should collate them somehow and report failure stats to the user.
fn walk_items(phase: &mut WalkParseExpandPhase, loc: &LocationMetadata, items: &[syn::Item]) {
    for item in items {
        let err = handle_relevant_item(phase, loc, item);
        if let Err(WalkError::PhaseIrrelevant) = err {
            let err = insert_into_db(&phase.db, loc, &item);
            if let Err(WalkError::Lower(LowerError::TypePositionMacro)) = err {
                unimplemented!()
            } else if let Err(WalkError::NonPub) = err {
                // TODO collate
            } else if let Err(err) = err {
                let span = Span::new(
                    loc.macro_invocation.clone(),
                    loc.source_file.clone(),
                    item.span(),
                );
                warn(err, &span);
            }
        }
    }
}

// handle items relevant to this phase: `use` statements, extern crate statements, and macros.
fn handle_relevant_item(
    phase: &mut WalkParseExpandPhase,
    loc: &LocationMetadata,
    item: &syn::Item,
) -> Result<(), WalkError> {
    let span = Span::new(
        loc.macro_invocation.clone(),
        loc.source_file.clone(),
        item.span(),
    );

    match item {
        syn::Item::ExternCrate(extern_crate) => {
            if loc.module_path.path.is_empty() {
                // crate root `extern crate`s have special semantics
                // and have already been handled in walk_crate
                return Ok(());
            }
            let metadata = lower_metadata(
                loc,
                &extern_crate.vis,
                &extern_crate.attrs,
                extern_crate.span(),
            )?;

            let ident = Ident::from(&extern_crate.ident);
            let crate_ = loc
                .crate_data
                .deps
                .get(&ident)
                .ok_or_else(|| WalkError::ExternCrateNotFound(ident.clone()))?;

            let scope = &mut phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope;

            let imports = match metadata.visibility {
                Visibility::Pub => &mut scope.pub_imports,
                Visibility::NonPub => &mut scope.imports,
            };

            let no_path: &[&str] = &[];
            phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope
                .imports
                .insert(ident, Identity::new(crate_.clone(), no_path).into());
        }
        syn::Item::Use(use_) => {
            let scope = &mut phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope;
            lower_use(scope, use_);
        }
        syn::Item::Macro(macro_) => {
            let target = UnresolvedPath::from(&macro_.mac.path);
            if let Some(ident) = target.get_ident() {
                if ident == &*MACRO_RULES {
                    let def = lower_macro_rules(&loc, &macro_)?;
                    trace!("found macro {}", def.name);
                    if def.macro_export {
                        let path =
                            Identity::new(loc.crate_data.crate_.clone(), &[def.name.clone()]);
                        phase
                            .db
                            .macros
                            .insert(path, MacroItem::Declarative(def.clone()))?;
                    }
                    // FIXME should this be in `else` or always happen?
                    let unexpanded_module = phase
                        .unexpanded_modules
                        .get_mut(&loc.module_path)
                        .expect("nonexistent module??");
                    unexpanded_module.textual_scope =
                        unexpanded_module.textual_scope.append_scope(Some(def));
                    return Ok(());
                }
            }

            let unexpanded_module = phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??");

            // not a macro_rules: save it for later
            unexpanded_module.unexpanded_items.push((
                span,
                unexpanded_module.textual_scope.append_scope(None),
                UnexpandedItem::UnresolvedMacroInvocation(Tokens::from(macro_)),
            ));
        }
        _ => return Err(WalkError::PhaseIrrelevant),
    }
    Ok(())
}

/// Walk an individual item.
/// If it's successfully shuffled into the db, return `Ok(())`. Otherwise, let
/// walk_items handle the error.
fn insert_into_db(db: &Db, loc: &LocationMetadata, item: &syn::Item) -> Result<(), WalkError> {
    let span = Span::new(
        loc.macro_invocation.clone(),
        loc.source_file.clone(),
        item.span(),
    );

    macro_rules! skip_non_pub {
        ($item:expr) => {
            match &$item.vis {
                syn::Visibility::Public(_) => (),
                _ => return Err(WalkError::NonPub),
            }
        };
    }

    match item {
        syn::Item::Static(static_) => {
            skip_non_pub!(static_);
            skip("static", loc.module_path.clone().join(&static_.ident))
            // TODO: add to scope when implemented
        }
        syn::Item::Const(const_) => {
            skip_non_pub!(const_);
            skip("const", loc.module_path.clone().join(&const_.ident))
            // TODO: add to scope when implemented
        }
        syn::Item::Fn(fn_) => {
            skip_non_pub!(fn_);
            let fn_ = lower_function_item(loc, fn_)?;
            db.symbols.insert(
                loc.module_path.clone().join(&fn_.name),
                SymbolItem::Function(fn_),
            )?;
        }
        // note: we don't skip non-items for the rest of this, since we may need to know about
        // all types for send + sync determination.
        // (in theory we might need non-pub info on the above items
        // for determining send+sync leakage on async closures but lol if i'm ever implementing that.)
        syn::Item::Type(type_) => skip("type", loc.module_path.clone().join(&type_.ident)),
        syn::Item::Struct(struct_) => {
            let struct_ = lower_struct(loc, struct_)?;
            db.types.insert(
                loc.module_path.clone().join(&struct_.name),
                TypeItem::Struct(struct_),
            )?;
        }
        syn::Item::Enum(enum_) => {
            let enum_ = lower_enum(loc, enum_)?;
            db.types.insert(
                loc.module_path.clone().join(&enum_.name),
                TypeItem::Enum(enum_),
            )?;
        }
        syn::Item::Union(union_) => skip("union", loc.module_path.clone().join(&union_.ident)),
        syn::Item::Trait(trait_) => skip("trait", loc.module_path.clone().join(&trait_.ident)),
        syn::Item::TraitAlias(alias_) => {
/*

/// Walk a whole crate, expanding macros, storing all resulting data in the central db.
/// This operates serially but multiple threads can operate on the same db in parallel.
fn walk_crate(crate_data: &CrateData, db: &Db) -> Result<(), WalkError> {
    trace!("walking {:?}", crate_data.crate_);

    let mut crate_data = crate_data.clone();
    let mut macro_prelude: Map<Ident, DeclarativeMacroItem> = Map::default();

    // Load the root entry
    let file = parse_file(&crate_data.entry)?;

    // Special handling: `extern crate`
    for item in &file.items {
        if let syn::Item::ExternCrate(extern_crate) = item {
            let result = (|| -> Result<(), WalkError> {
                let ident = Ident::from(&extern_crate.ident);
                let crate_ = ctx
                    .crate_data
                    .deps
                    .get(&ident)
                    .ok_or_else(|| WalkError::ExternCrateNotFound(ident.clone()))?;

                let macro_use = extern_crate.attrs.iter()
                    .any(|attr| attr.path.is_ident("macro_use"));

                if macro_use {
                    for item in db.macros.

                }

                if metadata.extract_attribute(&*MACRO_USE).is_some() {
                    // this miiiight have weird ordering consequences... whatever
                    ctx.unexpanded
                        .insert(UnexpandedItem::MacroUse(span.clone(), crate_.clone()));
                }

            })();

            /*
            fn handle_root_extern_crate(loc: &LocationMetadata, extern_crate: &syn::ItemExternCrate) {
                let mut metadata = lower_metadata(
                    loc,
                    &extern_crate.vis,
                    &extern_crate.attrs,
                    extern_crate.span(),
                )?;



                if let Some((_, name)) = &extern_crate.rename {
                    // add rename to crate namespace
                    // note that this effect *only* occurs at the crate root: otherwise `extern crate`
                    // just behaves like a `use`.
                    crate_data.deps.insert(Ident::from(&name), crate_.clone());
                }
            }
            */


        }
    }


    // special handling at crate root: extern crate
    let root_path = Identity::root(crate_data.crate_.clone());
    let source_root = crate_data.entry.parent().unwrap().to_path_buf();

    let mut phase = WalkParseExpandPhase {
        db: Db,
        unexpanded_modules: Map::default(),
        macro_prelude: Map::default()
    };


    // TODO extern crate extra effects at crate root:
    // - global alias
    // - #[macro_use] injects into all modules

    /*
    let root_module = ParsedModule {
        loc: LocationMetadata {
            source_file: &crate_data.entry.clone(),
            macro_invocation: None
        },
        items: vec![],
        scope: Default::default()
    };

    // prep modules
    let mut imports = ModuleScope::new();

    // the root context
    let mut ctx = WalkModuleCtx {
        source_file: crate_data.entry.clone(),
        module: root_path.clone(),
        db,
        scope: &mut imports,
        unexpanded: UnexpandedCursor::new(&mut unexpanded),
        crate_unexpanded_modules: &crate_unexpanded_modules,
        crate_data: &crate_data.clone(),
        macro_invocation: None,
    };

    // special case: walk the crate root looking for extern crates.
    // they behave special here for legacy reasons.

    // patch in data w/ modified extern crate names
    ctx.crate_data = crate_data;

    // get metadata for root crate entry
    let metadata = lower_metadata(&mut ctx, &syn::parse_quote!(pub), &file.attrs, file.span())?;
    let module = ModuleItem {
        name: Ident::from(root_path.crate_.name.to_string()),
        metadata,
    };

    // store results for root crate entry
    db.modules.insert(root_path.clone(), module)?;
    crate_unexpanded_modules.insert(root_path.clone(), unexpanded);

    //
    walk_items(&mut ctx, &file.items)?;

    let mut local_sequential_macros = Map::default();

    expand_module(
        db,
        &crate_unexpanded_modules,
        root_path.clone(),
        crate_data,
        &mut local_sequential_macros,
    )?;
    */

    Ok(())
}

/// Walk a list of items. They are stored in the phase data structures, either in `db` or
/// `unexpanded_modules`.
///
/// Note that this function cannot return an error! errors for individual items are currently just traced.
/// TODO: we should collate them somehow and report failure stats to the user.
fn walk_items(phase: &mut WalkParseExpandPhase, loc: &LocationMetadata, items: &[syn::Item]) {
    for item in items {
        let err = handle_relevant_item(phase, loc, item);
        if let Err(WalkError::PhaseIrrelevant) = err {
            let err = insert_into_db(&phase.db, loc, &item);
            if let Err(WalkError::Lower(LowerError::TypePositionMacro)) = err {
                unimplemented!()
            } else if let Err(WalkError::NonPub) = err {
                // TODO collate
            } else if let Err(err) = err {
                let span = Span::new(
                    loc.macro_invocation.clone(),
                    loc.source_file.clone(),
                    item.span(),
                );
                warn(err, &span);
            }
        }
    }
}

// handle items relevant to this phase: `use` statements, extern crate statements, and macros.
fn handle_relevant_item(
    phase: &mut WalkParseExpandPhase,
    loc: &LocationMetadata,
    item: &syn::Item,
) -> Result<(), WalkError> {
    let span = Span::new(
        loc.macro_invocation.clone(),
        loc.source_file.clone(),
        item.span(),
    );

    match item {
        syn::Item::ExternCrate(extern_crate) => {
            if loc.module_path.path.is_empty() {
                // crate root `extern crate`s have special semantics
                // and have already been handled in walk_crate
                return Ok(());
            }
            let metadata = lower_metadata(
                loc,
                &extern_crate.vis,
                &extern_crate.attrs,
                extern_crate.span(),
            )?;

            let ident = Ident::from(&extern_crate.ident);
            let crate_ = loc
                .crate_data
                .deps
                .get(&ident)
                .ok_or_else(|| WalkError::ExternCrateNotFound(ident.clone()))?;

            let scope = &mut phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope;

            let imports = match metadata.visibility {
                Visibility::Pub => &mut scope.pub_imports,
                Visibility::NonPub => &mut scope.imports,
            };

            let no_path: &[&str] = &[];
            phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope
                .imports
                .insert(ident, Identity::new(crate_.clone(), no_path).into());
        }
        syn::Item::Use(use_) => {
            let scope = &mut phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??")
                .scope;
            lower_use(scope, use_);
        }
        syn::Item::Macro(macro_) => {
            let target = UnresolvedPath::from(&macro_.mac.path);
            if let Some(ident) = target.get_ident() {
                if ident == &*MACRO_RULES {
                    let def = lower_macro_rules(&loc, &macro_)?;
                    trace!("found macro {}", def.name);
                    if def.macro_export {
                        let path =
                            Identity::new(loc.crate_data.crate_.clone(), &[def.name.clone()]);
                        phase
                            .db
                            .macros
                            .insert(path, MacroItem::Declarative(def.clone()))?;
                    }
                    // FIXME should this be in `else` or always happen?
                    let unexpanded_module = phase
                        .unexpanded_modules
                        .get_mut(&loc.module_path)
                        .expect("nonexistent module??");
                    unexpanded_module.textual_scope =
                        unexpanded_module.textual_scope.append_scope(Some(def));
                    return Ok(());
                }
            }

            let unexpanded_module = phase
                .unexpanded_modules
                .get_mut(&loc.module_path)
                .expect("nonexistent module??");

            // not a macro_rules: save it for later
            unexpanded_module.unexpanded_items.push((
                span,
                unexpanded_module.textual_scope.append_scope(None),
                UnexpandedItem::UnresolvedMacroInvocation(Tokens::from(macro_)),
            ));
        }
        _ => return Err(WalkError::PhaseIrrelevant),
    }
    Ok(())
}

/// Walk an individual item.
/// If it's successfully shuffled into the db, return `Ok(())`. Otherwise, let
/// walk_items handle the error.
fn insert_into_db(db: &Db, loc: &LocationMetadata, item: &syn::Item) -> Result<(), WalkError> {
    let span = Span::new(
        loc.macro_invocation.clone(),
        loc.source_file.clone(),
        item.span(),
    );

    macro_rules! skip_non_pub {
        ($item:expr) => {
            match &$item.vis {
                syn::Visibility::Public(_) => (),
                _ => return Err(WalkError::NonPub),
            }
        };
    }

    match item {
        syn::Item::Static(static_) => {
            skip_non_pub!(static_);
            skip("static", loc.module_path.clone().join(&static_.ident))
            // TODO: add to scope when implemented
        }
        syn::Item::Const(const_) => {
            skip_non_pub!(const_);
            skip("const", loc.module_path.clone().join(&const_.ident))
            // TODO: add to scope when implemented
        }
        syn::Item::Fn(fn_) => {
            skip_non_pub!(fn_);
            let fn_ = lower_function_item(loc, fn_)?;
            db.symbols.insert(
                loc.module_path.clone().join(&fn_.name),
                SymbolItem::Function(fn_),
            )?;
        }
        // note: we don't skip non-items for the rest of this, since we may need to know about
        // all types for send + sync determination.
        // (in theory we might need non-pub info on the above items
        // for determining send+sync leakage on async closures but lol if i'm ever implementing that.)
        syn::Item::Type(type_) => skip("type", loc.module_path.clone().join(&type_.ident)),
        syn::Item::Struct(struct_) => {
            let struct_ = lower_struct(loc, struct_)?;
            db.types.insert(
                loc.module_path.clone().join(&struct_.name),
                TypeItem::Struct(struct_),
            )?;
        }
        syn::Item::Enum(enum_) => {
            let enum_ = lower_enum(loc, enum_)?;
            db.types.insert(
                loc.module_path.clone().join(&enum_.name),
                TypeItem::Enum(enum_),
            )?;
        }
        syn::Item::Union(union_) => skip("union", loc.module_path.clone().join(&union_.ident)),
        syn::Item::Trait(trait_) => skip("trait", loc.module_path.clone().join(&trait_.ident)),
        syn::Item::TraitAlias(alias_) => {
            skip("trait alias", loc.module_path.clone().join(&alias_.ident))
        }
        syn::Item::Impl(_impl_) => skip("impl", loc.module_path.clone()),
        syn::Item::ForeignMod(_foreign_mod) => skip("foreign_mod", loc.module_path.clone()),
        syn::Item::Verbatim(_verbatim_) => skip("verbatim", loc.module_path.clone()),
        _ => (), // do nothing
    }
    Ok(())
}


*/     skip("trait alias", loc.module_path.clone().join(&alias_.ident))
        }
        syn::Item::Impl(_impl_) => skip("impl", loc.module_path.clone()),
        syn::Item::ForeignMod(_foreign_mod) => skip("foreign_mod", loc.module_path.clone()),
        syn::Item::Verbatim(_verbatim_) => skip("verbatim", loc.module_path.clone()),
        _ => (), // do nothing
    }
    Ok(())
}

/// The first phase: walk files, parse, find imports, expand macros.
/// As items cease to have anything to do with macros they are dumped in the Db; after macro
/// expansion order no longer matters.
struct WalkParseExpandPhase<'a> {
    /// A Db containing resolved definitions for all dependencies.
    /// During this phase, we insert `ModuleItems` and `MacroItems` into this database.
    /// The only items that go in here are `#[macro_export]`-marked macros.
    db: &'a Db,

    /// The relevant CrateData.
    crate_data: &'a CrateData,

    /// Declarative macro items inserted into the crate via `#[macro_use] extern crate`.
    /// Also has macros from the actual `std` / `core` prelude.
    ///
    /// `#[macro_export]` macros are NOT placed here.
    ///
    /// This information is discarded after parsing this crate.
    ///
    /// TODO: add std / core macros here?
    /// TODO: figure out what happens when we override those
    macro_prelude: Map<Ident, DeclarativeMacroItem>,

    /// The contents of modules we haven't finished expanding.
    unexpanded_modules: Map<Identity, UnexpandedModule<'a>>,
}
*/

/*
/// Walk a module.
fn parse_and_expand_module(
    phase: &ExpandPhase,
    path: Identity,
    local_sequential_macros: &mut Map<Ident, DeclarativeMacroItem>,
) -> Result<(), WalkError> {
    let unexpanded = unexpanded_modules.remove(&path);
    let mut unexpanded = if let Some((_, unexpanded)) = unexpanded {
        unexpanded
    } else {
        return Ok(());
    };

    // this won't cause errors because this function is never called more than once for a particular module
    db.scopes.take_modify(&path, |scope| {
        let mut ctx = WalkModuleCtx {
            source_file: unexpanded.source_file.clone(),
            module: path.clone(),
            db,
            scope,
            unexpanded: UnexpandedCursor::new(&mut unexpanded),
            // note: this is only used to insert submodules for macro expansion;
            // the fact that the current module is missing won't be a problem
            crate_unexpanded_modules: unexpanded_modules,
            crate_data,
            macro_invocation: None,
        };
        ctx.unexpanded.reset();

        while let Some(item) = ctx.unexpanded.pop() {
            let span = item.span().clone();

            // suppress errors here, don't return them
            let result = (|| -> Result<(), WalkError> {
                match item {
                    UnexpandedItem::MacroUse(_, crate_) => {
                        for path in db.macros.iter_crate(&crate_) {
                            db.macros.inspect(&path, |macro_| {
                                if let MacroItem::Declarative(macro_) = macro_ {
                                    local_sequential_macros
                                        .insert(macro_.name.clone(), macro_.clone());
                                }
                                Ok(())
                            })?;
                        }
                    }
                    UnexpandedItem::UnexpandedModule {
                        name, macro_use, ..
                    } => {
                        if macro_use {
                            expand_module(
                                db,
                                unexpanded_modules,
                                path.clone().join(name),
                                crate_data,
                                local_sequential_macros,
                            )?;
                        } else {
                            // TODO do we need to pass these in at all?
                            let mut local_sequential_macros = local_sequential_macros.clone();

                            expand_module(
                                db,
                                unexpanded_modules,
                                path.clone().join(name),
                                crate_data,
                                &mut local_sequential_macros,
                            )?;
                        }
                    }
                    UnexpandedItem::MacroInvocation(span, inv) => {
                        let inv = inv.parse::<syn::ItemMacro>().unwrap();

                        // TODO: attributes?

                        let target = UnresolvedPath::from(&inv.mac.path);

                        if let Some(ident) = target.get_ident() {
                            if ident == &*MACRO_RULES {
                                let def = lower_macro_rules(&ctx, &inv)?;
                                if def.macro_export {
                                    let path = Identity::new(
                                        ctx.crate_data.crate_.clone(),
                                        &[def.name.clone()],
                                    );
                                    db
                                        .macros
                                        .insert(path, MacroItem::Declarative(def.clone()))?;
                                }
                                trace!("found macro {}", def.name);
                                local_sequential_macros.insert(def.name.clone(), def);
                                return Ok(());
                            } else if let Some(macro_) = local_sequential_macros.get(ident) {
                                let result =
                                    crate::macro_interp::apply_once(macro_, inv.mac.tokens.clone())?;
                                let parsed: syn::File = syn::parse2(result)?;
                                trace!("applying macro {}! in {:?}", ident, path);

                                let result = walk_items(&mut ctx, &parsed.items[..]);
                                // make sure we macro_interp anything we discovered next
                                ctx.unexpanded.reset();

                                result?;

                                return Ok(());
                            }
                        }

                        warn!("[{:?}]: failed to resolve macro: {:?}", span, target);

                        // TODO: path lookups
                    }
                    UnexpandedItem::TypeMacro(span, _) => {
                        trace!("skipping type-position macro at {:?}, unimplemented", span)
                    }
                    UnexpandedItem::AttributeMacro(span, _) => {
                        trace!("skipping attribute macro at {:?}, unimplemented", span)
                    }
                    UnexpandedItem::DeriveMacro(span, _) => {
                        trace!("skipping #[derive] at {:?}, unimplemented", span)
                    }
                }
                Ok(())
            })();

            if let Err(err) = result {
                warn(err, &span)
            }
        }
        Ok(())
    })?;

    Ok(())
}
*/

/*
// A scope.
// Each scope currently corresponds to a module; that might change if we end up having to handle
// impl's in function scopes.
#[derive(Default)]
pub(crate) struct ModuleScope {
    /// This module's glob imports.
    /// `use x::y::z::*` is stored as `x::y::z` pre-resolution,
    /// and as an Identity post-resolution.
    /// Includes the prelude, if any.
    pub(crate) glob_imports: Vec<Path>,

    /// This module's non-glob imports.
    /// Maps the imported-as ident to a path,
    /// i.e. `use x::Y;` is stored as `Y => x::Y`,
    /// `use x::z as w` is stored as `w => x::z`
    pub(crate) imports: Map<Ident, Path>,

    /// This module's `pub` glob imports.
    /// `use x::y::z::*` is stored as `x::y::z` pre-resolution,
    /// and as an Identity post-resolution.
    /// Includes the prelude, if any.
    pub(crate) pub_glob_imports: Vec<Path>,

    /// This module's non-glob `pub` imports.
    /// Maps the imported-as ident to a path,
    /// i.e. `use x::Y;` is stored as `Y => x::Y`,
    /// `use x::z as w` is stored as `w => x::z`
    pub(crate) pub_imports: Map<Ident, Path>,
}
*/

/*
/// Context for lowering items in an individual module.
struct WalkModuleCtx<'a> {
    /// A Db containing resolved definitions for all dependencies.
    db: &'a Db,

    /// The location of this module's containing file in the filesystem.
    source_file: PathBuf,

    /// The module path.
    module: Identity,

    /// The scope for this module.
    scope: &'a mut ModuleScope,

    /// All items in this module that need to be macro-expanded.
    unexpanded: UnexpandedCursor<'a>,

    /// The metadata for the current crate, including imports.
    crate_data: &'a CrateData,

    /// If we are currently expanding a macro, the macro we're expanding from.
    macro_invocation: Option<Arc<Span>>,
}

impl ModuleScope {
    /// Create a new set of imports
    fn new() -> ModuleScope {
        ModuleScope {
            glob_imports: Vec::new(),
            imports: Map::default(),
            pub_glob_imports: Vec::new(),
            pub_imports: Map::default(),
        }
    }
}


fn merge_metadata(target: &mut Metadata, from: Metadata) {
    let Metadata {
        extra_attributes,
        deprecated,
        docs,
        ..
    } = from;

    target.metadata
        .extra_attributes
        .extend(extra_attributes.into_iter());
    if let (None, Some(deprecated)) = (&lowered.metadata.deprecated, deprecated) {
        target.metadata.deprecated = Some(deprecated);
    }
    if let (None, Some(docs)) = (&lowered.metadata.docs, docs) {
        target.metadata.docs = Some(docs);
    }
}

/// Parse a set of items into a database.
fn walk_items(ctx: &mut WalkModuleCtx, items: &[syn::Item]) -> Result<(), WalkError> {
    let _span = trace_span!("walk_items", path = tracing::field::debug(&loc.module_path));

    trace!("walking {:?}", loc.module_path);

    for item in items {
        let result = walk_item(ctx, item);

        if let Err(err) = result {
            let span = Span::new(
                ctx.macro_invocation.clone(),
                ctx.source_file.clone(),
                item.span(),
            );
            warn(err, &span)
        }
    }

    trace!("done walking items");

    Ok(())
}

fn walk_mod(ctx: &mut WalkModuleCtx, mod_: &syn::ItemMod) -> Result<(), WalkError> {
    // lower the ModuleItem (i.e. its attributes and stuff, not its contents)
    let mut lowered = lower_module(ctx, mod_)?;

    // the path of the submodule
    let path = loc.module_path.clone().join(&lowered.name);

    // borrowck juggling...
    let items: Vec<syn::Item>;
    let content: &[syn::Item];
    let source_file;

    if let Some((_, inline_content)) = &mod_.content {
        // the module conveniently provides its contents right here!
        content = &inline_content[..];
        source_file = ctx.source_file.clone();
    } else {
        // we gotta go find the module's source file.
        source_file = find_source_file(ctx, &mut lowered)?;
        // parse it.
        let parsed = parse_file(&source_file)?;

        // now that we've loaded the source file, let's attach its metadata to the original declaration.
        merge_metadata(&mut lowered.metadata,
            lower_metadata(&ctx, &syn::parse_quote!(), &parsed.attrs, parsed.span())?);

        // prep for recursion.
        let syn::File { items: items_, .. } = parsed;
        items = items_;
        content = &items[..];
    }

    let mut imports = ModuleScope::new();
    let mut unexpanded = UnexpandedModule::new(source_file.clone());

    {
        let mut ctx = WalkModuleCtx {
            source_file,
            crate_data: ctx.crate_data,
            module: loc.module_path.clone().join(lowered.name.clone()),
            scope: &mut imports,
            unexpanded: UnexpandedCursor::new(&mut unexpanded),
            db: &db,
            crate_unexpanded_modules: &ctx.crate_unexpanded_modules,
            macro_invocation: None,
        };

        // Invoke children
        walk_items(&mut ctx, content)?;

        trace!("finished invoking children");
    }

    trace!("insert modules");
    db.modules.insert(path.clone(), lowered)?;
    trace!("insert scopes");
    db.scopes.insert(path.clone(), imports)?;
    trace!("insert unexpanded");
    ctx.crate_unexpanded_modules
        .insert(path.clone(), unexpanded);

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use tendon_api::paths::AbsoluteCrate;

    #[test]
    fn walk_no_fs() {
        spoor::init();

        let db = Db::new();
        let source_file = PathBuf::from("/fake/fake_file.rs");
        let crate_ = AbsoluteCrate::new("fake_crate", "0.1.0");
        let mut scope = ModuleScope::new();
        let mut unexpanded = UnexpandedModule::new(source_file.clone());
        let crate_unexpanded_modules = DashMap::default();
        let mut ctx = WalkModuleCtx {
            module: Identity::root(crate_.clone()),
            source_file,
            db: &db,
            scope: &mut scope,
            unexpanded: UnexpandedCursor::new(&mut unexpanded),
            crate_unexpanded_modules: &crate_unexpanded_modules,
            crate_data: &CrateData::fake(),
            macro_invocation: None,
        };

        let fake: syn::File = syn::parse_quote! {
            extern crate bees as thing;

            // note: non-fns will be ignored
            fn f(y: i32) -> i32 {}

            enum X {}
            enum Y {}

            #[derive(Debug)]
            struct TestStruct {
                x: X, y: Y
            }
        };

        walk_items(&mut ctx, &fake.items).unwrap();

        assert!(db
            .symbols
            .contains(&Identity::new(crate_.clone(), &["f"])));
        assert!(db
            .types
            .contains(&Identity::new(crate_.clone(), &["TestStruct"])));
        assert!(db
            .types
            .contains(&Identity::new(crate_.clone(), &["X"])));
        assert!(db
            .types
            .contains(&Identity::new(crate_.clone(), &["Y"])));
    }
}
*/
