//! The Database data structure, which holds everything important about resolved code and is simple
//! to serialize to disk. Contains all the information needed for bindings generation.
//!
//! Important note: paths serve two purposes here. We identify items by their paths, but that's
//! not used for lookup when resolving code. All lookups go through the `bindings` tables in
//! `Namespace`. Every item has a binding corresponding to itself where it's introduced, but can of
//! course have other bindings. The `absolute_path` entry for each `Binding` tells you where
//! that item's stored in the `items` table of `Namespace` -- roughly,

use crate::attributes::{HasMetadata, Visibility};
use crate::crates::CrateData;
use crate::items::{MacroItem, ModuleItem, SymbolItem, TypeItem};
use crate::paths::{AbsoluteCrate, AbsolutePath, RelativePath};
use crate::Map;
use dashmap::DashMap;
use hashbrown::hash_map::Entry;
use serde::{Deserialize, Serialize};

/// A database of everything. Crates should form a DAG. Crates cannot be modified once added.
/// TODO: check for changes against disk?
#[derive(Serialize, Deserialize)]
pub struct Db {
    // TODO: this isn't serializeable w/ fxhash, make a PR for that
    // TODO: this could maybe be optimized w/ some sort of append-only representation
    crates: DashMap<AbsoluteCrate, CrateDb>,
}

impl Db {
    /// Create an empty database.
    pub fn new() -> Db {
        Db {
            crates: DashMap::default(),
        }
    }

    /// Add a crate to the database. The crate should be finished resolving.
    pub fn add_crate(&self, crate_db: CrateDb) {
        self.crates
            .insert(crate_db.crate_data.crate_.clone(), crate_db);
    }

    /// All items accessible via some crate. This is used to decide which items to bind.
    /// Returns a sequence of relative paths to bindings in the current crate, and the paths to the
    /// items they point to.
    ///
    /// If there are multiple bindings to some target, the shortest / lexicographically first is selected.
    /// Then, the whole list is sorted.
    /// This helps ensures determinism of generated bindings between runs.
    pub fn accessible_items<I: NamespaceLookup>(
        &self,
        crate_: &AbsoluteCrate,
    ) -> Vec<(RelativePath, AbsolutePath)> {
        let crate_db = self.crates.get(crate_).expect("no such crate");
        let namespace = I::get_crate_namespace(&crate_db);
        let mut result: Map<&AbsolutePath, &RelativePath> = Map::default();

        for (path, binding) in &namespace.bindings {
            let is_containing_module_public = path
                .parent()
                .map(|p| crate_db.is_module_externally_visible(&p))
                .unwrap_or(true);
            if binding.visibility == Visibility::Pub && is_containing_module_public {
                match result.entry(&binding.absolute_target) {
                    Entry::Vacant(v) => {
                        v.insert(path);
                    }
                    Entry::Occupied(mut o) => {
                        let should_replace = {
                            let cur_access_path = o.get_mut();

                            let new_shorter = path.0.len() < cur_access_path.0.len();
                            let new_same_and_lexicographically_earlier =
                                path.0.len() == cur_access_path.0.len() && path < cur_access_path;

                            new_shorter || new_same_and_lexicographically_earlier
                        };

                        if should_replace {
                            o.insert(path);
                        }
                    }
                }
            }
        }

        let mut result: Vec<(RelativePath, AbsolutePath)> = result
            .into_iter()
            .map(|(abs, rel)| (rel.clone(), abs.clone()))
            .collect();
        result.sort();
        result
    }
}

/// A database of everything found within a crate.
#[derive(Serialize, Deserialize)]
pub struct CrateDb {
    /// The crate's metadata.
    pub crate_data: CrateData,

    /// Types in the crate.
    types: CrateNamespace<TypeItem>,

    /// Symbols in the crate (functions, statics, constants)
    symbols: CrateNamespace<SymbolItem>,

    /// Macros in the crate.
    macros: CrateNamespace<MacroItem>,

    /// `mod` items, store metadata + privacy information, incl. the root module.
    modules: CrateNamespace<ModuleItem>,
}

impl CrateDb {
    /// Create a new database.
    pub fn new(crate_data: CrateData) -> CrateDb {
        CrateDb {
            types: CrateNamespace::new(crate_data.crate_.clone()),
            symbols: CrateNamespace::new(crate_data.crate_.clone()),
            macros: CrateNamespace::new(crate_data.crate_.clone()),
            modules: CrateNamespace::new(crate_data.crate_.clone()),
            crate_data,
        }
    }

    pub fn get_item<I: NamespaceLookup>(&self, path_: &RelativePath) -> Option<&I> {
        I::get_crate_namespace(self).items.get(path_)
    }

    pub fn get_item_mut<I: NamespaceLookup>(&mut self, path_: &RelativePath) -> Option<&mut I> {
        I::get_crate_namespace_mut(self).items.get_mut(path_)
    }

    pub fn get_binding<I: NamespaceLookup>(&self, path: &RelativePath) -> Option<&Binding> {
        I::get_crate_namespace(self).bindings.get(path)
    }

    /// Check if a module is externally visible.
    pub fn is_module_externally_visible(&self, mod_: &RelativePath) -> bool {
        let mut cur_check = RelativePath::root();
        for entry in &mod_.0 {
            cur_check.0.push(entry.clone()); // don't check root

            if self
                .get_binding::<ModuleItem>(&cur_check)
                .expect("checking missing module?")
                .visibility
                == Visibility::NonPub
            {
                return false;
            }
        }
        true
    }
}

/// A namespace within a crate, for holding some particular type of item during resolution.
/// `I` isn't constrained by `NamespaceLookup` for testing purposes but in effect it is.
#[derive(Serialize, Deserialize)]
pub struct CrateNamespace<I> {
    /// The AbsoluteCrate for this namespace.
    /// (stored redundantly for convenience.)
    crate_: AbsoluteCrate,

    /// True values, stored by the paths where they're defined. Note that this
    /// isn't used for binding lookups, just for storing actual values.
    items: Map<RelativePath, I>,

    /// Bindings.
    ///
    /// Note that every item has a binding added corresponding to itself within its module.
    ///
    /// Note also that these are collapsed: if you have `a reexports b reexports c`, this should map `a`
    /// to `c`, skipping `b`. This property is easy enough to ensure by construction.
    bindings: Map<RelativePath, Binding>,
}

impl<I: NamespaceLookup> CrateNamespace<I> {
    /// Create a namespace within a crate.
    fn new(crate_: AbsoluteCrate) -> Self {
        CrateNamespace {
            crate_,
            items: Map::default(),
            bindings: Map::default(),
        }
    }

    /// Insert an item, and add a binding for that item in the relevant module.
    pub fn add_item(&mut self, path: RelativePath, item: I) -> Result<(), DatabaseError> {
        let visibility = item.metadata().visibility;

        match self.items.entry(path.clone()) {
            Entry::Occupied(_) => return Err(DatabaseError::ItemAlreadyPresent),
            Entry::Vacant(v) => v.insert(item),
        };
        let target = AbsolutePath::new(self.crate_.clone(), &path.0);

        self.add_binding(path, target, visibility, Priority::Explicit)
    }

    /// Add a binding. Doesn't have to target something in this crate.
    pub fn add_binding(
        &mut self,
        path: RelativePath,
        target: AbsolutePath,
        visibility: Visibility,
        priority: Priority,
    ) -> Result<(), DatabaseError> {
        let mut binding = Binding {
            absolute_target: target,
            visibility,
            priority,
        };

        match self.bindings.entry(path) {
            Entry::Occupied(mut old) => {
                let old = old.get_mut();

                if old.priority == Priority::Glob && binding.priority == Priority::Explicit {
                    // TODO: signal that this occurred?
                    std::mem::swap(old, &mut binding);
                    Ok(())
                } else {
                    Err(DatabaseError::BindingAlreadyPresent)
                }
            }
            Entry::Vacant(v) => {
                v.insert(binding);
                Ok(())
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Glob,
    Explicit,
}

/// A name binding. (Nothing to do with the idea of "language bindings".)
#[derive(Serialize, Deserialize)]
pub struct Binding {
    /// The original, true path of the reexported item.
    pub absolute_target: AbsolutePath,

    /// The visibility of the binding (NOT the item), in the scope of its module.
    pub visibility: Visibility,

    /// If the binding is through a glob or explicit.
    pub priority: Priority,
}

/// A namespace.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Type,
    Symbol,
    Macro,
    Module,
}

/// Generic code helper.
pub trait NamespaceLookup: HasMetadata + Sized + 'static {
    fn namespace() -> Namespace;
    fn get_crate_namespace_mut(crate_db: &mut CrateDb) -> &mut CrateNamespace<Self>;
    fn get_crate_namespace(crate_db: &CrateDb) -> &CrateNamespace<Self>;
}
impl NamespaceLookup for TypeItem {
    fn namespace() -> Namespace {
        Namespace::Type
    }
    fn get_crate_namespace_mut(crate_db: &mut CrateDb) -> &mut CrateNamespace<Self> {
        &mut crate_db.types
    }
    fn get_crate_namespace(crate_db: &CrateDb) -> &CrateNamespace<Self> {
        &crate_db.types
    }
}
impl NamespaceLookup for SymbolItem {
    fn namespace() -> Namespace {
        Namespace::Symbol
    }
    fn get_crate_namespace_mut(crate_db: &mut CrateDb) -> &mut CrateNamespace<Self> {
        &mut crate_db.symbols
    }
    fn get_crate_namespace(crate_db: &CrateDb) -> &CrateNamespace<Self> {
        &crate_db.symbols
    }
}
impl NamespaceLookup for MacroItem {
    fn namespace() -> Namespace {
        Namespace::Macro
    }
    fn get_crate_namespace_mut(crate_db: &mut CrateDb) -> &mut CrateNamespace<Self> {
        &mut crate_db.macros
    }
    fn get_crate_namespace(crate_db: &CrateDb) -> &CrateNamespace<Self> {
        &crate_db.macros
    }
}
impl NamespaceLookup for ModuleItem {
    fn namespace() -> Namespace {
        Namespace::Module
    }
    fn get_crate_namespace_mut(crate_db: &mut CrateDb) -> &mut CrateNamespace<Self> {
        &mut crate_db.modules
    }
    fn get_crate_namespace(crate_db: &CrateDb) -> &CrateNamespace<Self> {
        &crate_db.modules
    }
}

quick_error::quick_error! {
    #[derive(Debug)]
    pub enum DatabaseError {
        ItemAlreadyPresent {
            display("item has already been added?")
        }
        BindingAlreadyPresent {
            display("item is already reexported?")
        }
    }
}