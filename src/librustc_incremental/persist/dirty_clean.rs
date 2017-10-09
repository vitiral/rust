// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Debugging code to test fingerprints computed for query results.
//! For each node marked with `#[rustc_clean]` or `#[rustc_dirty]`,
//! we will compare the fingerprint from the current and from the previous
//! compilation session as appropriate:
//!
//! - `#[rustc_dirty(label="TypeckTables", cfg="rev2")]` if we are
//!   in `#[cfg(rev2)]`, then the fingerprints associated with
//!   `DepNode::TypeckTables(X)` must be DIFFERENT (`X` is the def-id of the
//!   current node).
//! - `#[rustc_clean(label="TypeckTables", cfg="rev2")]` same as above,
//!   except that the fingerprints must be the SAME.
//!
//! Errors are reported if we are in the suitable configuration but
//! the required condition is not met.
//!
//! The `#[rustc_metadata_dirty]` and `#[rustc_metadata_clean]` attributes
//! can be used to check the incremental compilation hash (ICH) values of
//! metadata exported in rlibs.
//!
//! - If a node is marked with `#[rustc_metadata_clean(cfg="rev2")]` we
//!   check that the metadata hash for that node is the same for "rev2"
//!   it was for "rev1".
//! - If a node is marked with `#[rustc_metadata_dirty(cfg="rev2")]` we
//!   check that the metadata hash for that node is *different* for "rev2"
//!   than it was for "rev1".
//!
//! Note that the metadata-testing attributes must never specify the
//! first revision. This would lead to a crash since there is no
//! previous revision to compare things to.
//!

#![allow(dead_code)]

use std::collections::HashSet;
use std::iter::FromIterator;
use std::vec::Vec;
use rustc::dep_graph::{DepNode, label_strs};
use rustc::hir;
use rustc::hir::Item_ as HirItem;
use rustc::hir::map::Node as HirNode;
use rustc::hir::def_id::DefId;
use rustc::hir::itemlikevisit::ItemLikeVisitor;
use rustc::hir::intravisit;
use rustc::ich::{Fingerprint, ATTR_DIRTY, ATTR_CLEAN, ATTR_DIRTY_METADATA,
                 ATTR_CLEAN_METADATA};
use syntax::ast::{self, Attribute, NestedMetaItem};
use rustc_data_structures::fx::{FxHashSet, FxHashMap};
use syntax_pos::Span;
use rustc::ty::TyCtxt;

const EXCEPT: &str = "except";
const LABEL: &str = "label";
const CFG: &str = "cfg";

// Base and Extra labels to build up the labels
//
// FIXME(vitiral): still missing
// - mod declaration
// - external crate
// - external trait
// - foreign (external?) item
// - global asm

/// DepNodes for Hir, which is pretty much everything
const BASE_HIR: &[&str] = &[
    // Hir and HirBody should be computed for all nodes
    label_strs::Hir,
    label_strs::HirBody,
];

/// DepNodes for MirValidated/Optimized, which is relevant in "executable"
/// code, i.e. functions+methods
const BASE_MIR: &[&str] = &[
    label_strs::MirValidated,
    label_strs::MirOptimized,
];

/// DepNodes for functions + methods
const BASE_FN: &[&str] = &[
    // Callers will depend on the signature of these items, so we better test
    label_strs::TypeOfItem,
    label_strs::GenericsOfItem,
    label_strs::PredicatesOfItem,
    label_strs::FnSignature,

    // And a big part of compilation (that we eventually want to cache) is type inference
    // information:
    label_strs::TypeckTables,
];

/// extra DepNodes for methods (+fn)
const EXTRA_METHOD: &[&str] = &[
    label_strs::AssociatedItems,
];

/// extra DepNodes for trait-methods (+method+fn)
const EXTRA_TRAIT_METHOD: &[&str] = &[
    label_strs::TraitOfItem,
];

/// Struct, Enum and Union DepNodes
///
/// Note that changing the type of a field does not change the type of the struct or enum, but
/// adding/removing fields or changing a fields name or visibility does.
const BASE_STRUCT: &[&str] = &[
    label_strs::TypeOfItem,
    label_strs::GenericsOfItem,
    label_strs::PredicatesOfItem,
];

/// For typedef, constants, and statics
///
/// FIXME: question -- I split const/trait-method up and added TypeOfItem to these
const BASE_CONST: &[&str] = &[
    label_strs::TypeOfItem,
    label_strs::AssociatedItems,
    label_strs::TraitOfItem,
];

/// Trait definitions
const BASE_TRAIT: &[&str] = &[
    label_strs::TraitDefOfItem,
    label_strs::TraitImpls,
    label_strs::SpecializationGraph,
    label_strs::ObjectSafety,
    label_strs::AssociatedItemDefIds,
    label_strs::GenericsOfItem,
    label_strs::PredicatesOfItem,
];

/// `impl` implementation of struct/trait
const BASE_IMPL: &[&str] = &[
    label_strs::ImplTraitRef,
    label_strs::AssociatedItemDefIds,
    label_strs::GenericsOfItem,
];

// Fully Built Labels

/// Function DepNode
const LABELS_FN: &[&[&str]] = &[
    BASE_HIR,
    BASE_MIR,
    BASE_FN,
];

/// Method DepNodes
const LABELS_METHOD: &[&[&str]] = &[
    BASE_HIR,
    BASE_MIR,
    BASE_FN,
    EXTRA_METHOD,
];

/// Trait-Method DepNodes
const LABELS_TRAIT_METHOD: &[&[&str]] = &[
    BASE_HIR,
    BASE_MIR,
    BASE_FN,
    EXTRA_METHOD,
    EXTRA_TRAIT_METHOD,
];

/// Trait DepNodes
const LABELS_TRAIT: &[&[&str]] = &[
    BASE_HIR,
    BASE_TRAIT,
];

/// Impl DepNodes
const LABELS_IMPL: &[&[&str]] = &[
    BASE_HIR,
    BASE_IMPL,
];

/// Struct DepNodes
const LABELS_STRUCT: &[&[&str]] = &[
    BASE_HIR,
    BASE_STRUCT,
];

const LABELS_CONST: &[&[&str]] = &[
    BASE_HIR,
    BASE_CONST,
];

// FIXME: Struct/Enum/Unions Fields (there is currently no way to attach these)
//
// Fields are kind of separate from their containers, as they can change independently from
// them. We should at least check
//
//     TypeOfItem for these.

type Labels = HashSet<String>;

/// Represents the requested configuration by rustc_clean/dirty
struct Assertion {
    clean: Labels,
    dirty: Labels,
}

impl Assertion {
    fn from_clean_labels(labels: Labels) -> Assertion {
        Assertion {
            clean: labels,
            dirty: Labels::new(),
        }
    }

    fn from_dirty_labels(labels: Labels) -> Assertion {
        Assertion {
            clean: Labels::new(),
            dirty: labels,
        }
    }
}

pub fn check_dirty_clean_annotations<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>) {
    // can't add `#[rustc_dirty]` etc without opting in to this feature
    if !tcx.sess.features.borrow().rustc_attrs {
        return;
    }

    let _ignore = tcx.dep_graph.in_ignore();
    let krate = tcx.hir.krate();
    let mut dirty_clean_visitor = DirtyCleanVisitor {
        tcx,
        checked_attrs: FxHashSet(),
    };
    krate.visit_all_item_likes(&mut dirty_clean_visitor);

    let mut all_attrs = FindAllAttrs {
        tcx,
        attr_names: vec![ATTR_DIRTY, ATTR_CLEAN],
        found_attrs: vec![],
    };
    intravisit::walk_crate(&mut all_attrs, krate);

    // Note that we cannot use the existing "unused attribute"-infrastructure
    // here, since that is running before trans. This is also the reason why
    // all trans-specific attributes are `Whitelisted` in syntax::feature_gate.
    all_attrs.report_unchecked_attrs(&dirty_clean_visitor.checked_attrs);
}

pub struct DirtyCleanVisitor<'a, 'tcx:'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    checked_attrs: FxHashSet<ast::AttrId>,
}

impl<'a, 'tcx> DirtyCleanVisitor<'a, 'tcx> {

    /// Possibly "deserialize" the attribute into a clean/dirty assertion
    fn assertion_maybe(&mut self, item_id: ast::NodeId, attr: &Attribute)
        -> Option<Assertion>
    {
        let is_clean = if attr.check_name(ATTR_DIRTY) {
            false
        } else if attr.check_name(ATTR_CLEAN) {
            true
        } else {
            // skip: not rustc_clean/dirty
            return None
        };
        if !check_config(self.tcx, attr) {
            // skip: not the correct `cfg=`
            return None;
        }
        let assertion = if let Some(labels) = self.labels(attr) {
            if is_clean {
                Assertion::from_clean_labels(labels)
            } else {
                Assertion::from_dirty_labels(labels)
            }
        } else {
            self.assertion_auto(item_id, attr, is_clean)
        };
        Some(assertion)
    }

    /// Get the "auto" assertion on pre-validated attr, along with the `except` labels
    fn assertion_auto(&mut self, item_id: ast::NodeId, attr: &Attribute, is_clean: bool)
        -> Assertion
    {
        let (name, mut auto) = self.auto_labels(item_id, attr);
        let except = self.except(attr);
        for e in except.iter() {
            if !auto.remove(e) {
                let msg = format!(
                    "`except` specified DepNodes that can not be affected for \"{}\": \"{}\"",
                    name,
                    e
                );
                self.tcx.sess.span_fatal(attr.span, &msg);
            }
        }
        if is_clean {
            Assertion {
                clean: auto,
                dirty: except,
            }
        } else {
            Assertion {
                clean: except,
                dirty: auto,
            }
        }
    }

    fn labels(&self, attr: &Attribute) -> Option<Labels> {
        for item in attr.meta_item_list().unwrap_or_else(Vec::new) {
            if item.check_name(LABEL) {
                let value = expect_associated_value(self.tcx, &item);
                return Some(self.resolve_labels(&item, value.as_str().as_ref()));
            }
        }
        None
    }

    /// `except=` attribute value
    fn except(&self, attr: &Attribute) -> Labels {
        for item in attr.meta_item_list().unwrap_or_else(Vec::new) {
            if item.check_name(EXCEPT) {
                let value = expect_associated_value(self.tcx, &item);
                return self.resolve_labels(&item, value.as_str().as_ref());
            }
        }
        // if no `label` or `except` is given, only the node's group are asserted
        Labels::new()
    }

    /// Return all DepNode labels that should be asserted for this item.
    /// index=0 is the "name" used for error messages
    fn auto_labels(&mut self, item_id: ast::NodeId, attr: &Attribute) -> (&'static str, Labels) {
        let node = self.tcx.hir.get(item_id);
        let (name, labels) = match node {
            HirNode::NodeItem(item) => {
                match item.node {
                    // note: these are in the same order as hir::Item_;
                    // FIXME(vitiral): do commented out ones

                    /// An `extern crate` item, with optional original crate name,
                    // HirItem::ItemExternCrate(..),
                    /// `use foo::bar::*;` or `use foo::bar::baz as quux;`
                    // HirItem::ItemUse(..),
                    /// A `static` item
                    HirItem::ItemStatic(..) => ("ItemStatic", &LABELS_CONST),
                    /// A `const` item
                    HirItem::ItemConst(..) => ("ItemConst", &LABELS_CONST),
                    /// A function declaration (FIXME: standalone, impl and trait-impl??)
                    HirItem::ItemFn(..) => ("ItemFn", &LABELS_FN),
                    /// A module
                    // HirItem::ItemMod(..),
                    /// An external module
                    //HirItem::ItemForeignMod(..),
                    /// Module-level inline assembly (from global_asm!)
                    //HirItem::ItemGlobalAsm(..),
                    /// A type alias, e.g. `type Foo = Bar<u8>`
                    HirItem::ItemTy(..) => ("ItemTy", &LABELS_CONST),
                    /// An enum definition, e.g. `enum Foo<A, B> {C<A>, D<B>}`
                    HirItem::ItemEnum(..) => ("ItemEnum", &LABELS_STRUCT),
                    /// A struct definition, e.g. `struct Foo<A> {x: A}`
                    HirItem::ItemStruct(..) => ("ItemStruct", &LABELS_STRUCT),
                    /// A union definition, e.g. `union Foo<A, B> {x: A, y: B}`
                    HirItem::ItemUnion(..) => ("ItemUnion", &LABELS_STRUCT),
                    /// Represents a Trait Declaration
                    HirItem::ItemTrait(..) => ("ItemTrait", &LABELS_TRAIT),
                    /// `impl Trait for .. {}`
                    HirItem::ItemDefaultImpl(..) => ("ItemDefaultImpl", &LABELS_IMPL),
                    /// An implementation, eg `impl<A> Trait for Foo { .. }`
                    HirItem::ItemImpl(..) => ("ItemImpl", &LABELS_IMPL),

                    _ => self.tcx.sess.span_fatal(
                        attr.span,
                        &format!(
                            "clean/dirty auto-assertions not yet defined for NodeItem.node={:?}",
                            item.node
                        )
                    ),
                }
            },
            HirNode::NodeTraitItem(..) => ("NodeTraitItem", &LABELS_TRAIT_METHOD),
            HirNode::NodeImplItem(..) => ("NodeImplItem", &LABELS_METHOD),
            _ => self.tcx.sess.span_fatal(
                attr.span,
                &format!(
                    "clean/dirty auto-assertions not yet defined for {:?}",
                    node
                )
            ),
        };
        let labels = Labels::from_iter(
            labels.iter().flat_map(|s| s.iter().map(|l| l.to_string()))
        );
        (name, labels)
    }

    fn resolve_labels(&self, item: &NestedMetaItem, value: &str) -> Labels {
        let mut out: Labels = HashSet::new();
        for label in value.split(',') {
            let label = label.trim();
            if DepNode::has_label_string(label) {
                if out.contains(label) {
                    self.tcx.sess.span_fatal(
                        item.span,
                        &format!("dep-node label `{}` is repeated", label));
                }
                out.insert(label.to_string());
            } else {
                self.tcx.sess.span_fatal(
                    item.span,
                    &format!("dep-node label `{}` not recognized", label));
            }
        }
        out
    }

    fn dep_nodes(&self, labels: &Labels, def_id: DefId) -> Vec<DepNode> {
        let mut out = Vec::with_capacity(labels.len());
        let def_path_hash = self.tcx.def_path_hash(def_id);
        for label in labels.iter() {
            match DepNode::from_label_string(label, def_path_hash) {
                Ok(dep_node) => out.push(dep_node),
                Err(()) => unreachable!(),
            }
        }
        out
    }

    fn dep_node_str(&self, dep_node: &DepNode) -> String {
        if let Some(def_id) = dep_node.extract_def_id(self.tcx) {
            format!("{:?}({})",
                    dep_node.kind,
                    self.tcx.item_path_str(def_id))
        } else {
            format!("{:?}({:?})", dep_node.kind, dep_node.hash)
        }
    }

    fn assert_dirty(&self, item_span: Span, dep_node: DepNode) {
        debug!("assert_dirty({:?})", dep_node);

        let current_fingerprint = self.tcx.dep_graph.fingerprint_of(&dep_node);
        let prev_fingerprint = self.tcx.dep_graph.prev_fingerprint_of(&dep_node);

        if Some(current_fingerprint) == prev_fingerprint {
            let dep_node_str = self.dep_node_str(&dep_node);
            self.tcx.sess.span_err(
                item_span,
                &format!("`{}` should be dirty but is not", dep_node_str));
        }
    }

    fn assert_clean(&self, item_span: Span, dep_node: DepNode) {
        debug!("assert_clean({:?})", dep_node);

        let current_fingerprint = self.tcx.dep_graph.fingerprint_of(&dep_node);
        let prev_fingerprint = self.tcx.dep_graph.prev_fingerprint_of(&dep_node);

        if Some(current_fingerprint) != prev_fingerprint {
            let dep_node_str = self.dep_node_str(&dep_node);
            self.tcx.sess.span_err(
                item_span,
                &format!("`{}` should be clean but is not", dep_node_str));
        }
    }

    fn check_item(&mut self, item_id: ast::NodeId, item_span: Span) {
        let def_id = self.tcx.hir.local_def_id(item_id);
        for attr in self.tcx.get_attrs(def_id).iter() {
            let assertion = match self.assertion_maybe(item_id, attr) {
                Some(a) => a,
                None => continue,
            };
            self.checked_attrs.insert(attr.id);
            for dep_node in self.dep_nodes(&assertion.clean, def_id) {
                self.assert_clean(item_span, dep_node);
            }
            for dep_node in self.dep_nodes(&assertion.dirty, def_id) {
                self.assert_dirty(item_span, dep_node);
            }
        }
    }
}

impl<'a, 'tcx> ItemLikeVisitor<'tcx> for DirtyCleanVisitor<'a, 'tcx> {
    fn visit_item(&mut self, item: &'tcx hir::Item) {
        self.check_item(item.id, item.span);
    }

    fn visit_trait_item(&mut self, item: &hir::TraitItem) {
        self.check_item(item.id, item.span);
    }

    fn visit_impl_item(&mut self, item: &hir::ImplItem) {
        self.check_item(item.id, item.span);
    }
}

pub fn check_dirty_clean_metadata<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    prev_metadata_hashes: &FxHashMap<DefId, Fingerprint>,
    current_metadata_hashes: &FxHashMap<DefId, Fingerprint>)
{
    if !tcx.sess.opts.debugging_opts.query_dep_graph {
        return;
    }

    tcx.dep_graph.with_ignore(||{
        let krate = tcx.hir.krate();
        let mut dirty_clean_visitor = DirtyCleanMetadataVisitor {
            tcx,
            prev_metadata_hashes,
            current_metadata_hashes,
            checked_attrs: FxHashSet(),
        };
        intravisit::walk_crate(&mut dirty_clean_visitor, krate);

        let mut all_attrs = FindAllAttrs {
            tcx,
            attr_names: vec![ATTR_DIRTY_METADATA, ATTR_CLEAN_METADATA],
            found_attrs: vec![],
        };
        intravisit::walk_crate(&mut all_attrs, krate);

        // Note that we cannot use the existing "unused attribute"-infrastructure
        // here, since that is running before trans. This is also the reason why
        // all trans-specific attributes are `Whitelisted` in syntax::feature_gate.
        all_attrs.report_unchecked_attrs(&dirty_clean_visitor.checked_attrs);
    });
}

pub struct DirtyCleanMetadataVisitor<'a, 'tcx: 'a, 'm> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    prev_metadata_hashes: &'m FxHashMap<DefId, Fingerprint>,
    current_metadata_hashes: &'m FxHashMap<DefId, Fingerprint>,
    checked_attrs: FxHashSet<ast::AttrId>,
}

impl<'a, 'tcx, 'm> intravisit::Visitor<'tcx> for DirtyCleanMetadataVisitor<'a, 'tcx, 'm> {

    fn nested_visit_map<'this>(&'this mut self) -> intravisit::NestedVisitorMap<'this, 'tcx> {
        intravisit::NestedVisitorMap::All(&self.tcx.hir)
    }

    fn visit_item(&mut self, item: &'tcx hir::Item) {
        self.check_item(item.id, item.span);
        intravisit::walk_item(self, item);
    }

    fn visit_variant(&mut self,
                     variant: &'tcx hir::Variant,
                     generics: &'tcx hir::Generics,
                     parent_id: ast::NodeId) {
        if let Some(e) = variant.node.disr_expr {
            self.check_item(e.node_id, variant.span);
        }

        intravisit::walk_variant(self, variant, generics, parent_id);
    }

    fn visit_variant_data(&mut self,
                          variant_data: &'tcx hir::VariantData,
                          _: ast::Name,
                          _: &'tcx hir::Generics,
                          _parent_id: ast::NodeId,
                          span: Span) {
        if self.tcx.hir.find(variant_data.id()).is_some() {
            // VariantData that represent structs or tuples don't have a
            // separate entry in the HIR map and checking them would error,
            // so only check if this is an enum or union variant.
            self.check_item(variant_data.id(), span);
        }

        intravisit::walk_struct_def(self, variant_data);
    }

    fn visit_trait_item(&mut self, item: &'tcx hir::TraitItem) {
        self.check_item(item.id, item.span);
        intravisit::walk_trait_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'tcx hir::ImplItem) {
        self.check_item(item.id, item.span);
        intravisit::walk_impl_item(self, item);
    }

    fn visit_foreign_item(&mut self, i: &'tcx hir::ForeignItem) {
        self.check_item(i.id, i.span);
        intravisit::walk_foreign_item(self, i);
    }

    fn visit_struct_field(&mut self, s: &'tcx hir::StructField) {
        self.check_item(s.id, s.span);
        intravisit::walk_struct_field(self, s);
    }
}

impl<'a, 'tcx, 'm> DirtyCleanMetadataVisitor<'a, 'tcx, 'm> {

    fn check_item(&mut self, item_id: ast::NodeId, item_span: Span) {
        let def_id = self.tcx.hir.local_def_id(item_id);

        for attr in self.tcx.get_attrs(def_id).iter() {
            if attr.check_name(ATTR_DIRTY_METADATA) {
                if check_config(self.tcx, attr) {
                    if self.checked_attrs.insert(attr.id) {
                        self.assert_state(false, def_id, item_span);
                    }
                }
            } else if attr.check_name(ATTR_CLEAN_METADATA) {
                if check_config(self.tcx, attr) {
                    if self.checked_attrs.insert(attr.id) {
                        self.assert_state(true, def_id, item_span);
                    }
                }
            }
        }
    }

    fn assert_state(&self, should_be_clean: bool, def_id: DefId, span: Span) {
        let item_path = self.tcx.item_path_str(def_id);
        debug!("assert_state({})", item_path);

        if let Some(&prev_hash) = self.prev_metadata_hashes.get(&def_id) {
            let hashes_are_equal = prev_hash == self.current_metadata_hashes[&def_id];

            if should_be_clean && !hashes_are_equal {
                self.tcx.sess.span_err(
                        span,
                        &format!("Metadata hash of `{}` is dirty, but should be clean",
                                 item_path));
            }

            let should_be_dirty = !should_be_clean;
            if should_be_dirty && hashes_are_equal {
                self.tcx.sess.span_err(
                        span,
                        &format!("Metadata hash of `{}` is clean, but should be dirty",
                                 item_path));
            }
        } else {
            self.tcx.sess.span_err(
                        span,
                        &format!("Could not find previous metadata hash of `{}`",
                                 item_path));
        }
    }
}

/// Given a `#[rustc_dirty]` or `#[rustc_clean]` attribute, scan
/// for a `cfg="foo"` attribute and check whether we have a cfg
/// flag called `foo`.
///
/// Also make sure that the `label` and `except` fields do not
/// both exist.
fn check_config(tcx: TyCtxt, attr: &Attribute) -> bool {
    debug!("check_config(attr={:?})", attr);
    let config = &tcx.sess.parse_sess.config;
    debug!("check_config: config={:?}", config);
    let (mut cfg, mut except, mut label) = (None, false, false);
    for item in attr.meta_item_list().unwrap_or_else(Vec::new) {
        if item.check_name(CFG) {
            let value = expect_associated_value(tcx, &item);
            debug!("check_config: searching for cfg {:?}", value);
            cfg = Some(config.contains(&(value, None)));
        }
        if item.check_name(LABEL) {
            label = true;
        }
        if item.check_name(EXCEPT) {
            except = true;
        }
    }

    if label && except {
        tcx.sess.span_fatal(
            attr.span,
            "must specify only one of: `label`, `except`"
        );
    }

    match cfg {
        None => tcx.sess.span_fatal(
            attr.span,
            "no cfg attribute"
        ),
        Some(c) => c,
    }
}

fn expect_associated_value(tcx: TyCtxt, item: &NestedMetaItem) -> ast::Name {
    if let Some(value) = item.value_str() {
        value
    } else {
        let msg = if let Some(name) = item.name() {
            format!("associated value expected for `{}`", name)
        } else {
            "expected an associated value".to_string()
        };

        tcx.sess.span_fatal(item.span, &msg);
    }
}


// A visitor that collects all #[rustc_dirty]/#[rustc_clean] attributes from
// the HIR. It is used to verfiy that we really ran checks for all annotated
// nodes.
pub struct FindAllAttrs<'a, 'tcx:'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    attr_names: Vec<&'static str>,
    found_attrs: Vec<&'tcx Attribute>,
}

impl<'a, 'tcx> FindAllAttrs<'a, 'tcx> {

    fn is_active_attr(&mut self, attr: &Attribute) -> bool {
        for attr_name in &self.attr_names {
            if attr.check_name(attr_name) && check_config(self.tcx, attr) {
                return true;
            }
        }

        false
    }

    fn report_unchecked_attrs(&self, checked_attrs: &FxHashSet<ast::AttrId>) {
        for attr in &self.found_attrs {
            if !checked_attrs.contains(&attr.id) {
                self.tcx.sess.span_err(attr.span, &format!("found unchecked \
                    #[rustc_dirty]/#[rustc_clean] attribute"));
            }
        }
    }
}

impl<'a, 'tcx> intravisit::Visitor<'tcx> for FindAllAttrs<'a, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> intravisit::NestedVisitorMap<'this, 'tcx> {
        intravisit::NestedVisitorMap::All(&self.tcx.hir)
    }

    fn visit_attribute(&mut self, attr: &'tcx Attribute) {
        if self.is_active_attr(attr) {
            self.found_attrs.push(attr);
        }
    }
}
