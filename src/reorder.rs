//! Reorder items.
//!
//! `mod`, `extern crate` and `use` declarations are reordered in alphabetical
//! order. Trait items are reordered in pre-determined order (associated types
//! and constants comes before methods).

// FIXME(#2455): Reorder trait items.

use std::cmp::Ordering;

use rustc_ast::{ast, attr};
use rustc_span::{Span, symbol::sym};

use crate::StyleEdition;
use crate::config::{Config, GroupImportsTactic};
use crate::imports::{UseSegmentKind, UseTree, normalize_use_trees_with_granularity};
use crate::items::{is_mod_decl, rewrite_extern_crate, rewrite_mod};
use crate::lists::{ListFormatting, ListItem, itemize_list, write_list};
use crate::rewrite::{RewriteContext, RewriteError, RewriteResult};
use crate::shape::Shape;
use crate::sort::version_sort;
use crate::source_map::LineRangeUtils;
use crate::spanned::Spanned;
use crate::utils::{contains_skip, mk_sp};
use crate::visitor::FmtVisitor;

/// Choose the ordering between the given two items.
fn compare_items(a: &ast::Item, b: &ast::Item, context: &RewriteContext<'_>) -> Ordering {
    let style_edition = context.config.style_edition();
    match (&a.kind, &b.kind) {
        (&ast::ItemKind::Mod(_, a_ident, _), &ast::ItemKind::Mod(_, b_ident, _)) => {
            if style_edition <= StyleEdition::Edition2021 {
                a_ident.as_str().cmp(b_ident.as_str())
            } else {
                version_sort(a_ident.as_str(), b_ident.as_str())
            }
        }
        (
            &ast::ItemKind::ExternCrate(ref a_name, a_ident),
            &ast::ItemKind::ExternCrate(ref b_name, b_ident),
        ) => {
            // `extern crate foo as bar;`
            //               ^^^ Comparing this.
            let a_orig_name = a_name.unwrap_or(a_ident.name);
            let b_orig_name = b_name.unwrap_or(b_ident.name);
            let result = if style_edition <= StyleEdition::Edition2021 {
                a_orig_name.as_str().cmp(b_orig_name.as_str())
            } else {
                version_sort(a_orig_name.as_str(), b_orig_name.as_str())
            };
            if result != Ordering::Equal {
                return result;
            }

            // `extern crate foo as bar;`
            //                      ^^^ Comparing this.
            match (a_name, b_name) {
                (Some(..), None) => Ordering::Greater,
                (None, Some(..)) => Ordering::Less,
                (None, None) => Ordering::Equal,
                (Some(..), Some(..)) if style_edition <= StyleEdition::Edition2021 => {
                    a_ident.as_str().cmp(b_ident.as_str())
                }
                (Some(..), Some(..)) => version_sort(a_ident.as_str(), b_ident.as_str()),
            }
        }
        _ => unreachable!(),
    }
}

fn wrap_reorderable_items(
    context: &RewriteContext<'_>,
    list_items: &[ListItem],
    shape: Shape,
) -> RewriteResult {
    let fmt = ListFormatting::new(shape, context.config)
        .separator("")
        .align_comments(false);
    write_list(list_items, &fmt)
}

fn rewrite_reorderable_item(
    context: &RewriteContext<'_>,
    item: &ast::Item,
    shape: Shape,
) -> RewriteResult {
    match item.kind {
        ast::ItemKind::ExternCrate(..) => rewrite_extern_crate(context, item, shape),
        ast::ItemKind::Mod(_, ident, _) => rewrite_mod(context, item, ident, shape),
        _ => Err(RewriteError::Unknown),
    }
}

/// Rewrite a list of items with reordering and/or regrouping. Every item
/// in `items` must have the same `ast::ItemKind`. Whether reordering, regrouping,
/// or both are done is determined from the `context`.
fn rewrite_reorderable_or_regroupable_items(
    context: &RewriteContext<'_>,
    reorderable_items: &[&ast::Item],
    shape: Shape,
    span: Span,
) -> RewriteResult {
    match reorderable_items[0].kind {
        // FIXME: Remove duplicated code.
        ast::ItemKind::Use(..) => {
            let mut normalized_items: Vec<_> = reorderable_items
                .iter()
                .filter_map(|item| UseTree::from_ast_with_normalization(context, item))
                .collect();
            let cloned = normalized_items.clone();
            // Add comments before merging.
            let list_items = itemize_list(
                context.snippet_provider,
                cloned.iter(),
                "",
                ";",
                |item| item.span().lo(),
                |item| item.span().hi(),
                |_item| Ok("".to_owned()),
                span.lo(),
                span.hi(),
                false,
            );
            for (item, list_item) in normalized_items.iter_mut().zip(list_items) {
                item.list_item = Some(list_item.clone());
            }
            normalized_items = normalize_use_trees_with_granularity(
                normalized_items,
                context.config.imports_granularity(),
            );

            let mut regrouped_items = match context.config.group_imports() {
                GroupImportsTactic::Preserve | GroupImportsTactic::One => {
                    vec![normalized_items]
                }
                GroupImportsTactic::StdExternalCrate => group_imports(normalized_items),
            };

            if context.config.reorder_imports() {
                regrouped_items.iter_mut().for_each(|items| items.sort())
            }

            // 4 = "use ", 1 = ";"
            let nested_shape = shape.offset_left(4, span)?.sub_width(1, span)?;
            let item_vec: Vec<_> = regrouped_items
                .into_iter()
                .filter(|use_group| !use_group.is_empty())
                .map(|use_group| {
                    let item_vec: Vec<_> = use_group
                        .into_iter()
                        .map(|use_tree| {
                            let item = use_tree.rewrite_top_level(context, nested_shape);
                            if let Some(list_item) = use_tree.list_item {
                                ListItem {
                                    item: item,
                                    ..list_item
                                }
                            } else {
                                ListItem::from_item(item)
                            }
                        })
                        .collect();
                    wrap_reorderable_items(context, &item_vec, nested_shape)
                })
                .collect::<Result<Vec<_>, RewriteError>>()?;

            let join_string = format!("\n\n{}", shape.indent.to_string(context.config));
            Ok(item_vec.join(&join_string))
        }
        _ => {
            let list_items = itemize_list(
                context.snippet_provider,
                reorderable_items.iter(),
                "",
                ";",
                |item| item.span().lo(),
                |item| item.span().hi(),
                |item| rewrite_reorderable_item(context, item, shape),
                span.lo(),
                span.hi(),
                false,
            );

            let mut item_pair_vec: Vec<_> = list_items.zip(reorderable_items.iter()).collect();
            item_pair_vec.sort_by(|a, b| compare_items(a.1, b.1, context));
            let item_vec: Vec<_> = item_pair_vec.into_iter().map(|pair| pair.0).collect();

            wrap_reorderable_items(context, &item_vec, shape)
        }
    }
}

fn contains_macro_use_attr(item: &ast::Item) -> bool {
    attr::contains_name(&item.attrs, sym::macro_use)
}

/// Divides imports into three groups, corresponding to standard, external
/// and local imports. Sorts each subgroup.
fn group_imports(uts: Vec<UseTree>) -> Vec<Vec<UseTree>> {
    let mut std_imports = Vec::new();
    let mut external_imports = Vec::new();
    let mut local_imports = Vec::new();

    for ut in uts.into_iter() {
        if ut.path.is_empty() {
            external_imports.push(ut);
            continue;
        }
        match &ut.path[0].kind {
            UseSegmentKind::Ident(id, _) => match id.as_ref() {
                "std" | "alloc" | "core" => std_imports.push(ut),
                _ => external_imports.push(ut),
            },
            UseSegmentKind::Slf(_) | UseSegmentKind::Super(_) | UseSegmentKind::Crate(_) => {
                local_imports.push(ut)
            }
            // These are probably illegal here
            UseSegmentKind::Glob | UseSegmentKind::List(_) => external_imports.push(ut),
        }
    }

    vec![std_imports, external_imports, local_imports]
}

/// A simplified version of `ast::ItemKind`.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum ReorderableItemKind {
    ExternCrate,
    Mod,
    Use,
    /// An item that cannot be reordered. Either has an unreorderable item kind
    /// or an `macro_use` attribute.
    Other,
}

impl ReorderableItemKind {
    fn from(item: &ast::Item) -> Self {
        match item.kind {
            _ if contains_macro_use_attr(item) | contains_skip(&item.attrs) => {
                ReorderableItemKind::Other
            }
            ast::ItemKind::ExternCrate(..) => ReorderableItemKind::ExternCrate,
            ast::ItemKind::Mod(..) if is_mod_decl(item) => ReorderableItemKind::Mod,
            ast::ItemKind::Use(..) => ReorderableItemKind::Use,
            _ => ReorderableItemKind::Other,
        }
    }

    fn is_same_item_kind(self, item: &ast::Item) -> bool {
        ReorderableItemKind::from(item) == self
    }

    fn is_reorderable(self, config: &Config) -> bool {
        match self {
            ReorderableItemKind::ExternCrate => config.reorder_imports(),
            ReorderableItemKind::Mod => config.reorder_modules(),
            ReorderableItemKind::Use => config.reorder_imports(),
            ReorderableItemKind::Other => false,
        }
    }

    fn is_regroupable(self, config: &Config) -> bool {
        match self {
            ReorderableItemKind::ExternCrate
            | ReorderableItemKind::Mod
            | ReorderableItemKind::Other => false,
            ReorderableItemKind::Use => config.group_imports() != GroupImportsTactic::Preserve,
        }
    }

    fn in_group(self, config: &Config) -> bool {
        match self {
            ReorderableItemKind::ExternCrate | ReorderableItemKind::Mod => true,
            ReorderableItemKind::Use => config.group_imports() == GroupImportsTactic::Preserve,
            ReorderableItemKind::Other => false,
        }
    }
}

impl<'b, 'a: 'b> FmtVisitor<'a> {
    /// Format items with the same item kind and reorder them, regroup them, or
    /// both. If `in_group` is `true`, then the items separated by an empty line
    /// will not be reordered together.
    fn walk_reorderable_or_regroupable_items(
        &mut self,
        items: &[&ast::Item],
        item_kind: ReorderableItemKind,
        in_group: bool,
    ) -> usize {
        let mut last = self.psess.lookup_line_range(items[0].span());
        let item_length = items
            .iter()
            .take_while(|ppi| {
                item_kind.is_same_item_kind(&***ppi)
                    && (!in_group || {
                        let current = self.psess.lookup_line_range(ppi.span());
                        let in_same_group = current.lo < last.hi + 2;
                        last = current;
                        in_same_group
                    })
            })
            .count();
        let items = &items[..item_length];

        let at_least_one_in_file_lines = items
            .iter()
            .any(|item| !out_of_file_lines_range!(self, item.span));

        if at_least_one_in_file_lines && !items.is_empty() {
            let lo = items.first().unwrap().span().lo();
            let hi = items.last().unwrap().span().hi();
            let span = mk_sp(lo, hi);
            let rw = rewrite_reorderable_or_regroupable_items(
                &self.get_context(),
                items,
                self.shape(),
                span,
            );
            self.push_rewrite(span, rw.ok());
        } else {
            for item in items {
                self.push_rewrite(item.span, None);
            }
        }

        item_length
    }

    /// Visits and format the given items. Items are reordered If they are
    /// consecutive and reorderable.
    pub(crate) fn visit_items_with_reordering(&mut self, mut items: &[&ast::Item]) {
        let mut prev_item: Option<&ast::Item> = None;
        while !items.is_empty() {
            // If the next item is a `use`, `extern crate` or `mod`, then extract it and any
            // subsequent items that have the same item kind to be reordered within
            // `walk_reorderable_items`. Otherwise, just format the next item for output.
            let item_kind = ReorderableItemKind::from(items[0]);
            if item_kind.is_reorderable(self.config) || item_kind.is_regroupable(self.config) {
                let visited_items_num = self.walk_reorderable_or_regroupable_items(
                    items,
                    item_kind,
                    item_kind.in_group(self.config),
                );
                let (_, rest) = items.split_at(visited_items_num);
                // Insert blank line if needed
                if let Some(next) = rest.first() {
                    self.maybe_ensure_blank_between(prev_item, next);
                }
                if let Some(last) = items.get(visited_items_num - 1) {
                    prev_item = Some(*last);
                }
                items = rest;
            } else {
                // Reaching here means items were not reordered. There must be at least
                // one item left in `items`, so calling `unwrap()` here is safe.
                let (item, rest) = items.split_first().unwrap();
                // Insert blank line if needed
                self.maybe_ensure_blank_between(prev_item, item);
                self.visit_item(item);
                prev_item = Some(*item);
                items = rest;
            }
        }
    }

    /// Returns true if the given AST item is a top-level or item-like construct
    /// (e.g., function, struct, enum, module, trait, etc.) for which blank-line
    /// separation may be applied during formatting.
    fn is_item_like(item: &ast::Item) -> bool {
        matches!(
            item.kind,
            ast::ItemKind::Fn(..)
                | ast::ItemKind::Struct(..)
                | ast::ItemKind::Enum(..)
                | ast::ItemKind::Impl(..)
                | ast::ItemKind::Trait(..)
                | ast::ItemKind::Mod(..)
                | ast::ItemKind::Union(..)
                | ast::ItemKind::TyAlias(..)
                | ast::ItemKind::Const(..)
                | ast::ItemKind::Static(..)
                | ast::ItemKind::MacroDef(..)
                | ast::ItemKind::MacCall(..)
        )
    }

    /// Ensures that at least one blank line is inserted between two items.
    /// If the snippet between `prev` and `next` contains fewer than two newline
    /// characters, an extra newline is pushed to the output.
    fn ensure_blank_line_between_items(&mut self, prev: &ast::Item, next: &ast::Item) {
        let span_between = mk_sp(prev.span().hi(), next.span().lo());
        let gap = self.snippet(span_between);
        if crate::utils::count_newlines(gap) < 2 {
            self.push_str("\n");
        }
    }

    /// Inserts a blank line between two items if needed
    fn maybe_ensure_blank_between(&mut self, prev: Option<&ast::Item>, next: &ast::Item) {
        if let Some(prev_item) = prev {
            if Self::is_item_like(prev_item) || Self::is_item_like(next) {
                self.ensure_blank_line_between_items(prev_item, next);
            }
        }
    }
}
