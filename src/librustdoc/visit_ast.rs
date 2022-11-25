//! The Rust AST Visitor. Extracts useful information and massages it into a form
//! usable for `clean`.

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::{walk_item, Visitor};
use rustc_hir::Node;
use rustc_hir::CRATE_HIR_ID;
use rustc_middle::hir::map::Map;
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::{CRATE_DEF_ID, LOCAL_CRATE};
use rustc_span::symbol::{kw, sym, Symbol};
use rustc_span::Span;

use std::mem;

use crate::clean::{cfg::Cfg, AttributesExt, NestedAttributesExt};
use crate::core;

/// This module is used to store stuff from Rust's AST in a more convenient
/// manner (and with prettier names) before cleaning.
#[derive(Debug)]
pub(crate) struct Module<'hir> {
    pub(crate) name: Symbol,
    pub(crate) where_inner: Span,
    pub(crate) mods: Vec<Module<'hir>>,
    pub(crate) id: hir::HirId,
    // (item, renamed, import_id)
    pub(crate) items: Vec<(&'hir hir::Item<'hir>, Option<Symbol>, Option<hir::HirId>)>,
    pub(crate) foreigns: Vec<(&'hir hir::ForeignItem<'hir>, Option<Symbol>)>,
}

impl Module<'_> {
    pub(crate) fn new(name: Symbol, id: hir::HirId, where_inner: Span) -> Self {
        Module { name, id, where_inner, mods: Vec::new(), items: Vec::new(), foreigns: Vec::new() }
    }

    pub(crate) fn where_outer(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.hir().span(self.id)
    }
}

// FIXME: Should this be replaced with tcx.def_path_str?
fn def_id_to_path(tcx: TyCtxt<'_>, did: DefId) -> Vec<Symbol> {
    let crate_name = tcx.crate_name(did.krate);
    let relative = tcx.def_path(did).data.into_iter().filter_map(|elem| elem.data.get_opt_name());
    std::iter::once(crate_name).chain(relative).collect()
}

pub(crate) fn inherits_doc_hidden(tcx: TyCtxt<'_>, mut node: hir::HirId) -> bool {
    while let Some(id) = tcx.hir().get_enclosing_scope(node) {
        node = id;
        if tcx.hir().attrs(node).lists(sym::doc).has_word(sym::hidden) {
            return true;
        }
    }
    false
}

pub(crate) struct RustdocVisitor<'a, 'tcx> {
    cx: &'a mut core::DocContext<'tcx>,
    view_item_stack: FxHashSet<hir::HirId>,
    inlining: bool,
    /// Are the current module and all of its parents public?
    inside_public_path: bool,
    exact_paths: FxHashMap<DefId, Vec<Symbol>>,
    modules: Vec<Module<'tcx>>,
    map: Map<'tcx>,
}

impl<'a, 'tcx> RustdocVisitor<'a, 'tcx> {
    pub(crate) fn new(cx: &'a mut core::DocContext<'tcx>) -> RustdocVisitor<'a, 'tcx> {
        // If the root is re-exported, terminate all recursion.
        let mut stack = FxHashSet::default();
        stack.insert(hir::CRATE_HIR_ID);
        let om = Module::new(
            cx.tcx.crate_name(LOCAL_CRATE),
            hir::CRATE_HIR_ID,
            cx.tcx.hir().root_module().spans.inner_span,
        );
        let map = cx.tcx.hir();

        RustdocVisitor {
            cx,
            view_item_stack: stack,
            inlining: false,
            inside_public_path: true,
            exact_paths: FxHashMap::default(),
            modules: vec![om],
            map,
        }
    }

    fn store_path(&mut self, did: DefId) {
        let tcx = self.cx.tcx;
        self.exact_paths.entry(did).or_insert_with(|| def_id_to_path(tcx, did));
    }

    /// Tries to resolve the target of a `pub use` statement and inlines the
    /// target if it is defined locally and would not be documented otherwise,
    /// or when it is specifically requested with `please_inline`.
    /// (the latter is the case when the import is marked `doc(inline)`)
    ///
    /// Cross-crate inlining occurs later on during crate cleaning
    /// and follows different rules.
    ///
    /// Returns `true` if the target has been inlined.
    fn maybe_inline_local(
        &mut self,
        id: hir::HirId,
        res: Res,
        renamed: Option<Symbol>,
        glob: bool,
        please_inline: bool,
    ) -> bool {
        debug!("maybe_inline_local res: {:?}", res);

        if self.cx.output_format.is_json() {
            return false;
        }

        let tcx = self.cx.tcx;
        let Some(res_did) = res.opt_def_id() else {
            return false;
        };

        let use_attrs = tcx.hir().attrs(id);
        // Don't inline `doc(hidden)` imports so they can be stripped at a later stage.
        let is_no_inline = use_attrs.lists(sym::doc).has_word(sym::no_inline)
            || use_attrs.lists(sym::doc).has_word(sym::hidden);

        // For cross-crate impl inlining we need to know whether items are
        // reachable in documentation -- a previously unreachable item can be
        // made reachable by cross-crate inlining which we're checking here.
        // (this is done here because we need to know this upfront).
        if !res_did.is_local() && !is_no_inline {
            crate::visit_lib::lib_embargo_visit_item(self.cx, res_did);
            return false;
        }

        let res_hir_id = match res_did.as_local() {
            Some(n) => tcx.hir().local_def_id_to_hir_id(n),
            None => return false,
        };

        let is_private =
            !self.cx.cache.effective_visibilities.is_directly_public(self.cx.tcx, res_did);
        let is_hidden = inherits_doc_hidden(self.cx.tcx, res_hir_id);

        // Only inline if requested or if the item would otherwise be stripped.
        if (!please_inline && !is_private && !is_hidden) || is_no_inline {
            return false;
        }

        if !self.view_item_stack.insert(res_hir_id) {
            return false;
        }

        let ret = match tcx.hir().get(res_hir_id) {
            Node::Item(&hir::Item { kind: hir::ItemKind::Mod(ref m), .. }) if glob => {
                let prev = mem::replace(&mut self.inlining, true);
                for &i in m.item_ids {
                    let i = self.cx.tcx.hir().item(i);
                    self.visit_item_inner(i, None, Some(id));
                }
                self.inlining = prev;
                true
            }
            Node::Item(it) if !glob => {
                let prev = mem::replace(&mut self.inlining, true);
                self.visit_item_inner(it, renamed, Some(id));
                self.inlining = prev;
                true
            }
            Node::ForeignItem(it) if !glob => {
                let prev = mem::replace(&mut self.inlining, true);
                self.visit_foreign_item_inner(it, renamed);
                self.inlining = prev;
                true
            }
            _ => false,
        };
        self.view_item_stack.remove(&res_hir_id);
        ret
    }

    #[inline]
    fn add_to_current_mod(
        &mut self,
        item: &'tcx hir::Item<'_>,
        renamed: Option<Symbol>,
        parent_id: Option<hir::HirId>,
    ) {
        self.modules.last_mut().unwrap().items.push((item, renamed, parent_id))
    }

    fn visit_item_inner(
        &mut self,
        item: &'tcx hir::Item<'_>,
        renamed: Option<Symbol>,
        parent_id: Option<hir::HirId>,
    ) -> bool {
        debug!("visiting item {:?}", item);
        let name = renamed.unwrap_or(item.ident.name);

        let def_id = item.owner_id.to_def_id();
        let is_pub = self.cx.tcx.visibility(def_id).is_public();

        if is_pub {
            self.store_path(item.owner_id.to_def_id());
        }

        match item.kind {
            hir::ItemKind::ForeignMod { items, .. } => {
                for item in items {
                    let item = self.cx.tcx.hir().foreign_item(item.id);
                    self.visit_foreign_item_inner(item, None);
                }
            }
            // If we're inlining, skip private items or item reexported as "_".
            _ if self.inlining && (!is_pub || renamed == Some(kw::Underscore)) => {}
            hir::ItemKind::GlobalAsm(..) => {}
            hir::ItemKind::Use(_, hir::UseKind::ListStem) => {}
            hir::ItemKind::Use(path, kind) => {
                for &res in &path.res {
                    // Struct and variant constructors and proc macro stubs always show up alongside
                    // their definitions, we've already processed them so just discard these.
                    if let Res::Def(DefKind::Ctor(..), _) | Res::SelfCtor(..) = res {
                        continue;
                    }

                    let attrs = self.cx.tcx.hir().attrs(item.hir_id());

                    // If there was a private module in the current path then don't bother inlining
                    // anything as it will probably be stripped anyway.
                    if is_pub && self.inside_public_path {
                        let please_inline = attrs.iter().any(|item| match item.meta_item_list() {
                            Some(ref list) if item.has_name(sym::doc) => {
                                list.iter().any(|i| i.has_name(sym::inline))
                            }
                            _ => false,
                        });
                        let is_glob = kind == hir::UseKind::Glob;
                        let ident = if is_glob { None } else { Some(name) };
                        if self.maybe_inline_local(
                            item.hir_id(),
                            res,
                            ident,
                            is_glob,
                            om,
                            please_inline,
                        ) {
                            continue;
                        }
                    }

                    self.add_to_current_mod(item, renamed, parent_id);
                }
            }
            hir::ItemKind::Macro(ref macro_def, _) => {
                // `#[macro_export] macro_rules!` items are handled separately in `visit()`,
                // above, since they need to be documented at the module top level. Accordingly,
                // we only want to handle macros if one of three conditions holds:
                //
                // 1. This macro was defined by `macro`, and thus isn't covered by the case
                //    above.
                // 2. This macro isn't marked with `#[macro_export]`, and thus isn't covered
                //    by the case above.
                // 3. We're inlining, since a reexport where inlining has been requested
                //    should be inlined even if it is also documented at the top level.

                let def_id = item.owner_id.to_def_id();
                let is_macro_2_0 = !macro_def.macro_rules;
                let nonexported = !self.cx.tcx.has_attr(def_id, sym::macro_export);

                if is_macro_2_0 || nonexported || self.inlining {
                    self.add_to_current_mod(item, renamed, None);
                }
            }
            hir::ItemKind::Mod(ref m) => {
                self.enter_mod(item.hir_id(), m, name);
            }
            hir::ItemKind::Fn(..)
            | hir::ItemKind::ExternCrate(..)
            | hir::ItemKind::Enum(..)
            | hir::ItemKind::Struct(..)
            | hir::ItemKind::Union(..)
            | hir::ItemKind::TyAlias(..)
            | hir::ItemKind::OpaqueTy(..)
            | hir::ItemKind::Static(..)
            | hir::ItemKind::Trait(..)
            | hir::ItemKind::TraitAlias(..) => {
                self.add_to_current_mod(item, renamed, parent_id);
            }
            hir::ItemKind::Const(..) => {
                // Underscore constants do not correspond to a nameable item and
                // so are never useful in documentation.
                if name != kw::Underscore {
                    self.add_to_current_mod(item, renamed, parent_id);
                }
            }
            hir::ItemKind::Impl(impl_) => {
                // Don't duplicate impls when inlining or if it's implementing a trait, we'll pick
                // them up regardless of where they're located.
                if !self.inlining && impl_.of_trait.is_none() {
                    self.add_to_current_mod(item, None, None);
                }
            }
        }
        true
    }

    fn visit_foreign_item_inner(
        &mut self,
        item: &'tcx hir::ForeignItem<'_>,
        renamed: Option<Symbol>,
    ) {
        // If inlining we only want to include public functions.
        if !self.inlining || self.cx.tcx.visibility(item.owner_id).is_public() {
            self.modules.last_mut().unwrap().foreigns.push((item, renamed));
        }
    }

    pub(crate) fn visit(mut self) -> Module<'tcx> {
        let root_module = self.cx.tcx.hir().root_module();
        self.visit_mod_contents(CRATE_HIR_ID, root_module);

        let mut top_level_module = self.modules.pop().unwrap();

        // `#[macro_export] macro_rules!` items are reexported at the top level of the
        // crate, regardless of where they're defined. We want to document the
        // top level rexport of the macro, not its original definition, since
        // the rexport defines the path that a user will actually see. Accordingly,
        // we add the rexport as an item here, and then skip over the original
        // definition in `visit_item()` below.
        //
        // We also skip `#[macro_export] macro_rules!` that have already been inserted,
        // it can happen if within the same module a `#[macro_export] macro_rules!`
        // is declared but also a reexport of itself producing two exports of the same
        // macro in the same module.
        let mut inserted = FxHashSet::default();
        for export in self.cx.tcx.module_reexports(CRATE_DEF_ID).unwrap_or(&[]) {
            if let Res::Def(DefKind::Macro(_), def_id) = export.res &&
                let Some(local_def_id) = def_id.as_local() &&
                self.cx.tcx.has_attr(def_id, sym::macro_export) &&
                inserted.insert(def_id)
            {
                    let item = self.cx.tcx.hir().expect_item(local_def_id);
                    top_level_module.items.push((item, None, None));
            }
        }

        self.cx.cache.hidden_cfg = self
            .cx
            .tcx
            .hir()
            .attrs(CRATE_HIR_ID)
            .iter()
            .filter(|attr| attr.has_name(sym::doc))
            .flat_map(|attr| attr.meta_item_list().into_iter().flatten())
            .filter(|attr| attr.has_name(sym::cfg_hide))
            .flat_map(|attr| {
                attr.meta_item_list()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|attr| {
                        Cfg::parse(attr.meta_item()?)
                            .map_err(|e| self.cx.sess().diagnostic().span_err(e.span, e.msg))
                            .ok()
                    })
                    .collect::<Vec<_>>()
            })
            .chain(
                [Cfg::Cfg(sym::test, None), Cfg::Cfg(sym::doc, None), Cfg::Cfg(sym::doctest, None)]
                    .into_iter(),
            )
            .collect();

        self.cx.cache.exact_paths = self.exact_paths;
        top_level_module
    }

    /// This method will create a new module and push it onto the "modules stack" then call
    /// `visit_mod_contents`. Once done, it'll remove it from the "modules stack" and instead
    /// add into into the list of modules of the current module.
    fn enter_mod(&mut self, id: hir::HirId, m: &'tcx hir::Mod<'tcx>, name: Symbol) {
        self.modules.push(Module::new(name, id, m.spans.inner_span));

        self.visit_mod_contents(id, m);

        let last = self.modules.pop().unwrap();
        self.modules.last_mut().unwrap().mods.push(last);
    }

    /// This method will go through the given module items in two passes:
    /// 1. The items which are not glob imports/reexports.
    /// 2. The glob imports/reexports.
    fn visit_mod_contents(&mut self, id: hir::HirId, m: &'tcx hir::Mod<'tcx>) {
        debug!("Going through module {:?}", m);
        let def_id = self.cx.tcx.hir().local_def_id(id).to_def_id();
        // Keep track of if there were any private modules in the path.
        let orig_inside_public_path = self.inside_public_path;
        self.inside_public_path &= self.cx.tcx.visibility(def_id).is_public();

        // Reimplementation of `walk_mod`:
        for &i in m.item_ids {
            let item = self.cx.tcx.hir().item(i);
            if !matches!(item.kind, hir::ItemKind::Use(_, hir::UseKind::Glob)) {
                self.visit_item(item);
            }
        }
        for &i in m.item_ids {
            let item = self.cx.tcx.hir().item(i);
            // To match the way import precedence works, visit glob imports last.
            // Later passes in rustdoc will de-duplicate by name and kind, so if glob-
            // imported items appear last, then they'll be the ones that get discarded.
            if matches!(item.kind, hir::ItemKind::Use(_, hir::UseKind::Glob)) {
                self.visit_item(item);
            }
        }
        self.inside_public_path = orig_inside_public_path;
    }
}

// We need to implement this visitor so it'll go everywhere and retrieve items we're interested in
// such as impl blocks in const blocks.
impl<'a, 'tcx> Visitor<'tcx> for RustdocVisitor<'a, 'tcx> {
    type NestedFilter = nested_filter::All;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.map
    }

    fn visit_item(&mut self, i: &'tcx hir::Item<'tcx>) {
        let parent_id = if self.modules.len() > 1 {
            Some(self.modules[self.modules.len() - 2].id)
        } else {
            None
        };
        if self.visit_item_inner(i, None, parent_id) {
            walk_item(self, i);
        }
    }

    fn visit_mod(&mut self, _: &hir::Mod<'tcx>, _: Span, _: hir::HirId) {
        // handled in `visit_item_inner`
    }
}
