/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A centralized set of stylesheets for a document.

use dom::TElement;
use invalidation::stylesheets::StylesheetInvalidationSet;
use media_queries::Device;
use selector_parser::SnapshotMap;
use shared_lock::SharedRwLockReadGuard;
use std::slice;
use stylesheets::{Origin, OriginSet, OriginSetIterator, PerOrigin, StylesheetInDocument};

/// Entry for a StylesheetSet.
#[cfg_attr(feature = "servo", derive(MallocSizeOf))]
struct StylesheetSetEntry<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// The sheet.
    sheet: S,

    /// Whether this sheet has been part of at least one flush.
    committed: bool,
}

impl<S> StylesheetSetEntry<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    fn new(sheet: S) -> Self {
        Self { sheet, committed: false }
    }
}

/// A iterator over the stylesheets of a list of entries in the StylesheetSet.
pub struct StylesheetCollectionIterator<'a, S>(slice::Iter<'a, StylesheetSetEntry<S>>)
where
    S: StylesheetInDocument + PartialEq + 'static;

impl<'a, S> Clone for StylesheetCollectionIterator<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    fn clone(&self) -> Self {
        StylesheetCollectionIterator(self.0.clone())
    }
}


impl<'a, S> Iterator for StylesheetCollectionIterator<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    type Item = &'a S;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|entry| &entry.sheet)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// An iterator over the flattened view of the stylesheet collections.
#[derive(Clone)]
pub struct StylesheetIterator<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    origins: OriginSetIterator,
    collections: &'a PerOrigin<SheetCollection<S>>,
    current: Option<(Origin, StylesheetCollectionIterator<'a, S>)>,
}

impl<'a, S> Iterator for StylesheetIterator<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    type Item = (&'a S, Origin);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let next_origin = self.origins.next()?;

                self.current =
                    Some((next_origin, self.collections.borrow_for_origin(&next_origin).iter()));
            }

            {
                let (origin, ref mut iter) = *self.current.as_mut().unwrap();
                if let Some(s) = iter.next() {
                    return Some((s, origin))
                }
            }

            self.current = None;
        }
    }
}

/// The validity of the data in a given cascade origin.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "servo", derive(MallocSizeOf))]
pub enum DataValidity {
    /// The origin is clean, all the data already there is valid, though we may
    /// have new sheets at the end.
    Valid = 0,

    /// The cascade data is invalid, but not the invalidation data (which is
    /// order-independent), and thus only the cascade data should be inserted.
    CascadeInvalid = 1,

    /// Everything needs to be rebuilt.
    FullyInvalid = 2,
}

impl Default for DataValidity {
    fn default() -> Self {
        DataValidity::Valid
    }
}

/// A struct to iterate over the different stylesheets to be flushed.
pub struct StylesheetFlusher<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    origins_dirty: OriginSet,
    collections: &'a mut PerOrigin<SheetCollection<S>>,
    origin_data_validity: PerOrigin<DataValidity>,
    had_invalidations: bool,
}

/// The type of rebuild that we need to do for a given stylesheet.
#[derive(Clone, Copy, Debug)]
pub enum SheetRebuildKind {
    /// A full rebuild, of both cascade data and invalidation data.
    Full,
    /// A partial rebuild, of only the cascade data.
    CascadeOnly,
}

impl SheetRebuildKind {
    /// Whether the stylesheet invalidation data should be rebuilt.
    pub fn should_rebuild_invalidation(&self) -> bool {
        matches!(*self, SheetRebuildKind::Full)
    }
}

impl<'a, S> StylesheetFlusher<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// The data validity for a given origin.
    pub fn data_validity(&self, origin: Origin) -> DataValidity {
        *self.origin_data_validity.borrow_for_origin(&origin)
    }

    /// Whether the origin data is dirty in any way.
    pub fn origin_dirty(&self, origin: Origin) -> bool {
        self.origins_dirty.contains(origin.into())
    }

    /// Returns an iterator over the stylesheets of a given origin, assuming all
    /// of them will be flushed.
    pub fn manual_origin_sheets<'b>(&'b mut self, origin: Origin) -> StylesheetCollectionIterator<'b, S>
    where
        'a: 'b
    {
        debug_assert_eq!(origin, Origin::UserAgent);

        // We could iterate over `origin_sheets(origin)` to ensure state is
        // consistent (that the `committed` member of the Entry is reset to
        // `true`).
        //
        // In practice it doesn't matter for correctness given our use of it
        // (that this is UA only).
        //
        // FIXME(emilio): I think it does matter and that we effectively don't
        // support removing UA sheets since #19927...
        self.collections.borrow_for_origin(&origin).iter()
    }

    /// Returns a flusher for the dirty origin `origin`.
    pub fn origin_sheets<'b>(&'b mut self, origin: Origin) -> PerOriginFlusher<'b, S>
    where
        'a: 'b
    {
        let validity = self.data_validity(origin);
        let origin_dirty = self.origins_dirty.contains(origin.into());

        debug_assert!(
            origin_dirty || validity == DataValidity::Valid,
            "origin_data_validity should be a subset of origins_dirty!"
        );

        PerOriginFlusher {
            iter: self.collections.borrow_mut_for_origin(&origin).entries.iter_mut(),
            validity,
        }
    }

    /// Returns whether running the whole flushing process would be a no-op.
    pub fn nothing_to_do(&self) -> bool {
        self.origins_dirty.is_empty()
    }

    /// Returns whether any DOM invalidations were processed as a result of the
    /// stylesheet flush.
    pub fn had_invalidations(&self) -> bool {
        self.had_invalidations
    }
}

/// A flusher struct for a given origin, that takes care of returning the
/// appropriate stylesheets that need work.
pub struct PerOriginFlusher<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static
{
    iter: slice::IterMut<'a, StylesheetSetEntry<S>>,
    validity: DataValidity,
}

impl<'a, S> Iterator for PerOriginFlusher<'a, S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    type Item = (&'a S, SheetRebuildKind);

    fn next(&mut self) -> Option<Self::Item> {
        use std::mem;

        loop {
            let potential_sheet = self.iter.next()?;

            let committed = mem::replace(&mut potential_sheet.committed, true);
            if !committed {
                // If the sheet was uncommitted, we need to do a full rebuild
                // anyway.
                return Some((&potential_sheet.sheet, SheetRebuildKind::Full))
            }

            let rebuild_kind = match self.validity {
                DataValidity::Valid => continue,
                DataValidity::CascadeInvalid => SheetRebuildKind::CascadeOnly,
                DataValidity::FullyInvalid => SheetRebuildKind::Full,
            };

            return Some((&potential_sheet.sheet, rebuild_kind));
        }
    }
}

#[cfg_attr(feature = "servo", derive(MallocSizeOf))]
struct SheetCollection<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// The actual list of stylesheets.
    ///
    /// This is only a list of top-level stylesheets, and as such it doesn't
    /// include recursive `@import` rules.
    entries: Vec<StylesheetSetEntry<S>>,

    /// The validity of the data that was already there for a given origin.
    ///
    /// Note that an origin may appear on `origins_dirty`, but still have
    /// `DataValidity::Valid`, if only sheets have been appended into it (in
    /// which case the existing data is valid, but the origin needs to be
    /// rebuilt).
    data_validity: DataValidity,

    /// Whether anything in the collection has changed. Note that this is
    /// different from `data_validity`, in the sense that after a sheet append,
    /// the data validity is still `Valid`, but we need to be marked as dirty.
    dirty: bool,
}

impl<S> Default for SheetCollection<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    fn default() -> Self {
        Self {
            entries: vec![],
            data_validity: DataValidity::Valid,
            dirty: false,
        }
    }
}

impl<S> SheetCollection<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// Returns the number of stylesheets in the set.
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the `index`th stylesheet in the set if present.
    fn get(&self, index: usize) -> Option<&S> {
        self.entries.get(index).map(|e| &e.sheet)
    }

    fn remove(&mut self, sheet: &S) {
        let index = self.entries.iter().position(|entry| {
            entry.sheet == *sheet
        });
        if cfg!(feature = "gecko") && index.is_none() {
            // FIXME(emilio): Make Gecko's PresShell::AddUserSheet not suck.
            return;
        }
        let sheet = self.entries.remove(index.unwrap());
        // Removing sheets makes us tear down the whole cascade and invalidation
        // data, but only if the sheet has been involved in at least one flush.
        // Checking whether the sheet has been committed allows us to avoid
        // rebuilding the world when sites quickly append and remove a stylesheet.
        // See bug 1434756.
        if sheet.committed {
            self.set_data_validity_at_least(DataValidity::FullyInvalid);
        } else {
            self.dirty = true;
        }
    }

    fn contains(&self, sheet: &S) -> bool {
        self.entries.iter().any(|e| e.sheet == *sheet)
    }

    /// Appends a given sheet into the collection.
    fn append(&mut self, sheet: S) {
        debug_assert!(!self.contains(&sheet));
        self.entries.push(StylesheetSetEntry::new(sheet));
        // Appending sheets doesn't alter the validity of the existing data, so
        // we don't need to change `data_validity` here.
        //
        // But we need to be marked as dirty, otherwise we'll never add the new
        // sheet!
        self.dirty = true;
    }

    fn insert_before(&mut self, sheet: S, before_sheet: &S) {
        debug_assert!(!self.contains(&sheet));

        let index = self.entries.iter().position(|entry| {
            entry.sheet == *before_sheet
        }).expect("`before_sheet` stylesheet not found");

        // Inserting stylesheets somewhere but at the end changes the validity
        // of the cascade data, but not the invalidation data.
        self.set_data_validity_at_least(DataValidity::CascadeInvalid);
        self.entries.insert(index, StylesheetSetEntry::new(sheet));
    }

    fn set_data_validity_at_least(&mut self, validity: DataValidity) {
        use std::cmp;

        debug_assert_ne!(validity, DataValidity::Valid);

        self.dirty = true;
        self.data_validity = cmp::max(validity, self.data_validity);
    }

    fn prepend(&mut self, sheet: S) {
        debug_assert!(!self.contains(&sheet));
        // Inserting stylesheets somewhere but at the end changes the validity
        // of the cascade data, but not the invalidation data.
        self.set_data_validity_at_least(DataValidity::CascadeInvalid);
        self.entries.insert(0, StylesheetSetEntry::new(sheet));
    }

    /// Returns an iterator over the current list of stylesheets.
    fn iter(&self) -> StylesheetCollectionIterator<S> {
        StylesheetCollectionIterator(self.entries.iter())
    }
}

/// The set of stylesheets effective for a given document.
#[cfg_attr(feature = "servo", derive(MallocSizeOf))]
pub struct DocumentStylesheetSet<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// The collections of sheets per each origin.
    collections: PerOrigin<SheetCollection<S>>,

    /// The invalidations for stylesheets added or removed from this document.
    invalidations: StylesheetInvalidationSet,
}

/// This macro defines methods common to DocumentStylesheetSet and
/// AuthorStylesheetSet.
///
/// We could simplify the setup moving invalidations to SheetCollection, but
/// that would imply not sharing invalidations across origins of the same
/// documents, which is slightly annoying.
macro_rules! sheet_set_methods {
    ($set_name:expr) => {
        fn collect_invalidations_for(
            &mut self,
            device: Option<&Device>,
            sheet: &S,
            guard: &SharedRwLockReadGuard,
        ) {
            if let Some(device) = device {
                self.invalidations.collect_invalidations_for(device, sheet, guard);
            }
        }

        /// Appends a new stylesheet to the current set.
        ///
        /// No device implies not computing invalidations.
        pub fn append_stylesheet(
            &mut self,
            device: Option<&Device>,
            sheet: S,
            guard: &SharedRwLockReadGuard
        ) {
            debug!(concat!($set_name, "::append_stylesheet"));
            self.collect_invalidations_for(device, &sheet, guard);
            let collection = self.collection_for(&sheet, guard);
            collection.append(sheet);
        }

        /// Prepend a new stylesheet to the current set.
        pub fn prepend_stylesheet(
            &mut self,
            device: Option<&Device>,
            sheet: S,
            guard: &SharedRwLockReadGuard
        ) {
            debug!(concat!($set_name, "::prepend_stylesheet"));
            self.collect_invalidations_for(device, &sheet, guard);

            let collection = self.collection_for(&sheet, guard);
            collection.prepend(sheet);
        }

        /// Insert a given stylesheet before another stylesheet in the document.
        pub fn insert_stylesheet_before(
            &mut self,
            device: Option<&Device>,
            sheet: S,
            before_sheet: S,
            guard: &SharedRwLockReadGuard,
        ) {
            debug!(concat!($set_name, "::insert_stylesheet_before"));
            self.collect_invalidations_for(device, &sheet, guard);

            let collection = self.collection_for(&sheet, guard);
            collection.insert_before(sheet, &before_sheet);
        }

        /// Remove a given stylesheet from the set.
        pub fn remove_stylesheet(
            &mut self,
            device: Option<&Device>,
            sheet: S,
            guard: &SharedRwLockReadGuard,
        ) {
            debug!(concat!($set_name, "::remove_stylesheet"));
            self.collect_invalidations_for(device, &sheet, guard);

            let collection = self.collection_for(&sheet, guard);
            collection.remove(&sheet)
        }
    }
}

impl<S> DocumentStylesheetSet<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// Create a new empty StylesheetSet.
    pub fn new() -> Self {
        Self {
            collections: Default::default(),
            invalidations: StylesheetInvalidationSet::new(),
        }
    }

    fn collection_for(
        &mut self,
        sheet: &S,
        guard: &SharedRwLockReadGuard,
    ) -> &mut SheetCollection<S> {
        let origin = sheet.contents(guard).origin;
        self.collections.borrow_mut_for_origin(&origin)
    }

    sheet_set_methods!("DocumentStylesheetSet");

    /// Returns the number of stylesheets in the set.
    pub fn len(&self) -> usize {
        self.collections.iter_origins().fold(0, |s, (item, _)| s + item.len())
    }

    /// Returns the `index`th stylesheet in the set for the given origin.
    pub fn get(&self, origin: Origin, index: usize) -> Option<&S> {
        self.collections.borrow_for_origin(&origin).get(index)
    }

    /// Returns whether the given set has changed from the last flush.
    pub fn has_changed(&self) -> bool {
        self.collections.iter_origins().any(|(collection, _)| collection.dirty)
    }

    /// Flush the current set, unmarking it as dirty, and returns a
    /// `StylesheetFlusher` in order to rebuild the stylist.
    pub fn flush<'a, E>(
        &'a mut self,
        document_element: Option<E>,
        snapshots: Option<&SnapshotMap>,
    ) -> StylesheetFlusher<'a, S>
    where
        E: TElement,
    {
        use std::mem;

        debug!("DocumentStylesheetSet::flush");

        let had_invalidations =
            self.invalidations.flush(document_element, snapshots);

        let mut origins_dirty = OriginSet::empty();
        let mut origin_data_validity = PerOrigin::<DataValidity>::default();
        for (collection, origin) in self.collections.iter_mut_origins() {
            let was_dirty = mem::replace(&mut collection.dirty, false);
            if !was_dirty {
                debug_assert_eq!(collection.data_validity, DataValidity::Valid);
                continue;
            }

            origins_dirty |= origin;
            *origin_data_validity.borrow_mut_for_origin(&origin) =
                mem::replace(&mut collection.data_validity, DataValidity::Valid);
        }

        StylesheetFlusher {
            collections: &mut self.collections,
            had_invalidations,
            origins_dirty,
            origin_data_validity,
        }
    }

    /// Flush stylesheets, but without running any of the invalidation passes.
    #[cfg(feature = "servo")]
    pub fn flush_without_invalidation(&mut self) -> OriginSet {
        use std::mem;

        debug!("DocumentStylesheetSet::flush_without_invalidation");

        self.invalidations.clear();
        for (collection, origin) in self.collections.iter_mut_origins() {
            collection.dirty = false;
            // NOTE(emilio): I think this should also poke at the data validity
            // and such, but it doesn't really matter given we don't use that
            // collection for style resolution anyway.
        }
    }

    /// Return an iterator over the flattened view of all the stylesheets.
    pub fn iter(&self) -> StylesheetIterator<S> {
        StylesheetIterator {
            origins: OriginSet::all().iter(),
            collections: &self.collections,
            current: None,
        }
    }

    /// Mark the stylesheets for the specified origin as dirty, because
    /// something external may have invalidated it.
    pub fn force_dirty(&mut self, origins: OriginSet) {
        self.invalidations.invalidate_fully();
        for origin in origins.iter() {
            // We don't know what happened, assume the worse.
            self.collections
                .borrow_mut_for_origin(&origin)
                .set_data_validity_at_least(DataValidity::FullyInvalid);
        }
    }
}

/// The set of stylesheets effective for a given XBL binding or Shadow Root.
pub struct AuthorStylesheetSet<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    /// The actual style sheets.
    collection: SheetCollection<S>,
    /// The set of invalidations scheduled for this collection.
    invalidations: StylesheetInvalidationSet,
}

impl<S> AuthorStylesheetSet<S>
where
    S: StylesheetInDocument + PartialEq + 'static,
{
    fn collection_for(
        &mut self,
        _sheet: &S,
        _guard: &SharedRwLockReadGuard,
    ) -> &mut SheetCollection<S> {
        &mut self.collection
    }

    sheet_set_methods!("AuthorStylesheetSet");
}
