use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek};

use fxhash::FxHashMap;
use heed::RoTxn;
use itertools::Itertools;
use obkv::{KvReader, KvReaderU16, KvWriter};
use roaring::RoaringBitmap;
use serde_json::Value;
use smartstring::SmartString;

use super::helpers::{
    create_sorter, create_writer, obkvs_keep_last_addition_merge_deletions,
    obkvs_merge_additions_and_deletions, MergeFn,
};
use super::{IndexDocumentsMethod, IndexerConfig};
use crate::documents::{DocumentsBatchIndex, EnrichedDocument, EnrichedDocumentsBatchReader};
use crate::error::{Error, InternalError, UserError};
use crate::index::{db_name, main_key};
use crate::update::del_add::into_del_add_obkv;
use crate::update::{AvailableDocumentsIds, ClearDocuments, UpdateIndexingStep};
use crate::{
    FieldDistribution, FieldId, FieldIdMapMissingEntry, FieldsIdsMap, Index, Result, BEU32,
};

pub struct TransformOutput {
    pub primary_key: String,
    pub fields_ids_map: FieldsIdsMap,
    pub field_distribution: FieldDistribution,
    pub new_external_documents_ids: fst::Map<Cow<'static, [u8]>>,
    pub new_documents_ids: RoaringBitmap,
    pub replaced_documents_ids: RoaringBitmap,
    pub documents_count: usize,
    pub original_documents: File,
    pub flattened_documents: File,
}

/// Extract the external ids, deduplicate and compute the new internal documents ids
/// and fields ids, writing all the documents under their internal ids into a final file.
///
/// Outputs the new `FieldsIdsMap`, the new `UsersIdsDocumentsIds` map, the new documents ids,
/// the replaced documents ids, the number of documents in this update and the file
/// containing all those documents.
pub struct Transform<'a, 'i> {
    pub index: &'i Index,
    fields_ids_map: FieldsIdsMap,

    indexer_settings: &'a IndexerConfig,
    pub autogenerate_docids: bool,
    pub index_documents_method: IndexDocumentsMethod,
    available_documents_ids: AvailableDocumentsIds,

    // Both grenad follows the same format:
    // key | value
    // u32 | 1 byte for the Operation byte, the rest is the obkv of the document stored
    original_sorter: grenad::Sorter<MergeFn>,
    flattened_sorter: grenad::Sorter<MergeFn>,

    replaced_documents_ids: RoaringBitmap,
    new_documents_ids: RoaringBitmap,
    // To increase the cache locality and decrease the heap usage we use compact smartstring.
    new_external_documents_ids_builder: FxHashMap<SmartString<smartstring::Compact>, u64>,
    documents_count: usize,
}

/// This enum is specific to the grenad sorter stored in the transform.
/// It's used as the first byte of the grenads and tells you if the document id was an addition or a deletion.
#[repr(u8)]
pub enum Operation {
    Addition,
    Deletion,
}

/// Create a mapping between the field ids found in the document batch and the one that were
/// already present in the index.
///
/// If new fields are present in the addition, they are added to the index field ids map.
fn create_fields_mapping(
    index_field_map: &mut FieldsIdsMap,
    batch_field_map: &DocumentsBatchIndex,
) -> Result<HashMap<FieldId, FieldId>> {
    batch_field_map
        .iter()
        // we sort by id here to ensure a deterministic mapping of the fields, that preserves
        // the original ordering.
        .sorted_by_key(|(&id, _)| id)
        .map(|(field, name)| match index_field_map.id(name) {
            Some(id) => Ok((*field, id)),
            None => index_field_map
                .insert(name)
                .ok_or(Error::UserError(UserError::AttributeLimitReached))
                .map(|id| (*field, id)),
        })
        .collect()
}

impl<'a, 'i> Transform<'a, 'i> {
    pub fn new(
        wtxn: &mut heed::RwTxn,
        index: &'i Index,
        indexer_settings: &'a IndexerConfig,
        index_documents_method: IndexDocumentsMethod,
        autogenerate_docids: bool,
    ) -> Result<Self> {
        // We must choose the appropriate merge function for when two or more documents
        // with the same user id must be merged or fully replaced in the same batch.
        let merge_function = match index_documents_method {
            IndexDocumentsMethod::ReplaceDocuments => obkvs_keep_last_addition_merge_deletions,
            IndexDocumentsMethod::UpdateDocuments => obkvs_merge_additions_and_deletions,
        };

        // We initialize the sorter with the user indexing settings.
        let original_sorter = create_sorter(
            grenad::SortAlgorithm::Stable,
            merge_function,
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            indexer_settings.max_nb_chunks,
            indexer_settings.max_memory.map(|mem| mem / 2),
        );

        // We initialize the sorter with the user indexing settings.
        let flattened_sorter = create_sorter(
            grenad::SortAlgorithm::Stable,
            merge_function,
            indexer_settings.chunk_compression_type,
            indexer_settings.chunk_compression_level,
            indexer_settings.max_nb_chunks,
            indexer_settings.max_memory.map(|mem| mem / 2),
        );
        let documents_ids = index.documents_ids(wtxn)?;
        let soft_deleted_documents_ids = index.soft_deleted_documents_ids(wtxn)?;

        Ok(Transform {
            index,
            fields_ids_map: index.fields_ids_map(wtxn)?,
            indexer_settings,
            autogenerate_docids,
            available_documents_ids: AvailableDocumentsIds::from_documents_ids(
                &documents_ids,
                &soft_deleted_documents_ids,
            ),
            original_sorter,
            flattened_sorter,
            index_documents_method,
            replaced_documents_ids: RoaringBitmap::new(),
            new_documents_ids: RoaringBitmap::new(),
            new_external_documents_ids_builder: FxHashMap::default(),
            documents_count: 0,
        })
    }

    pub fn read_documents<R, FP, FA>(
        &mut self,
        reader: EnrichedDocumentsBatchReader<R>,
        wtxn: &mut heed::RwTxn,
        progress_callback: FP,
        should_abort: FA,
    ) -> Result<usize>
    where
        R: Read + Seek,
        FP: Fn(UpdateIndexingStep) + Sync,
        FA: Fn() -> bool + Sync,
    {
        let (mut cursor, fields_index) = reader.into_cursor_and_fields_index();
        let external_documents_ids = self.index.external_documents_ids(wtxn)?;
        let mapping = create_fields_mapping(&mut self.fields_ids_map, &fields_index)?;

        let primary_key = cursor.primary_key().to_string();
        let primary_key_id =
            self.fields_ids_map.insert(&primary_key).ok_or(UserError::AttributeLimitReached)?;

        let mut obkv_buffer = Vec::new();
        let mut document_sorter_buffer = Vec::new();
        let mut documents_count = 0;
        let mut docid_buffer: Vec<u8> = Vec::new();
        let mut field_buffer: Vec<(u16, Cow<[u8]>)> = Vec::new();
        while let Some(enriched_document) = cursor.next_enriched_document()? {
            let EnrichedDocument { document, document_id } = enriched_document;

            if should_abort() {
                return Err(Error::InternalError(InternalError::AbortedIndexation));
            }

            // drop_and_reuse is called instead of .clear() to communicate to the compiler that field_buffer
            // does not keep references from the cursor between loop iterations
            let mut field_buffer_cache = drop_and_reuse(field_buffer);
            if self.indexer_settings.log_every_n.map_or(false, |len| documents_count % len == 0) {
                progress_callback(UpdateIndexingStep::RemapDocumentAddition {
                    documents_seen: documents_count,
                });
            }

            // When the document id has been auto-generated by the `enrich_documents_batch`
            // we must insert this document id into the remaped document.
            let external_id = document_id.value();
            if document_id.is_generated() {
                serde_json::to_writer(&mut docid_buffer, external_id)
                    .map_err(InternalError::SerdeJson)?;
                field_buffer_cache.push((primary_key_id, Cow::from(&docid_buffer)));
            }

            for (k, v) in document.iter() {
                let mapped_id =
                    *mapping.get(&k).ok_or(InternalError::FieldIdMappingMissingEntry { key: k })?;
                field_buffer_cache.push((mapped_id, Cow::from(v)));
            }

            // Insertion in a obkv need to be done with keys ordered. For now they are ordered
            // according to the document addition key order, so we sort it according to the
            // fieldids map keys order.
            field_buffer_cache.sort_unstable_by(|(f1, _), (f2, _)| f1.cmp(f2));

            // Build the new obkv document.
            let mut writer = obkv::KvWriter::new(&mut obkv_buffer);
            for (k, v) in field_buffer_cache.iter() {
                writer.insert(*k, v)?;
            }

            let mut original_docid = None;

            let docid = match self.new_external_documents_ids_builder.entry((*external_id).into()) {
                Entry::Occupied(entry) => *entry.get() as u32,
                Entry::Vacant(entry) => {
                    let docid = match external_documents_ids.get(entry.key()) {
                        Some(docid) => {
                            // If it was already in the list of replaced documents it means it was deleted
                            // by the remove_document method. We should starts as if it never existed.
                            if self.replaced_documents_ids.insert(docid) {
                                original_docid = Some(docid);
                            }

                            docid
                        }
                        None => self
                            .available_documents_ids
                            .next()
                            .ok_or(UserError::DocumentLimitReached)?,
                    };
                    entry.insert(docid as u64);
                    docid
                }
            };

            let mut skip_insertion = false;
            if let Some(original_docid) = original_docid {
                let original_key = BEU32::new(original_docid);
                let base_obkv = self
                    .index
                    .documents
                    .remap_data_type::<heed::types::ByteSlice>()
                    .get(wtxn, &original_key)?
                    .ok_or(InternalError::DatabaseMissingEntry {
                        db_name: db_name::DOCUMENTS,
                        key: None,
                    })?;

                // we check if the two documents are exactly equal. If it's the case we can skip this document entirely
                if base_obkv == obkv_buffer {
                    // we're not replacing anything
                    self.replaced_documents_ids.remove(original_docid);
                    // and we need to put back the original id as it was before
                    self.new_external_documents_ids_builder.remove(external_id);
                    skip_insertion = true;
                } else {
                    // we associate the base document with the new key, everything will get merged later.
                    let keep_original_version =
                        self.index_documents_method == IndexDocumentsMethod::UpdateDocuments;
                    document_sorter_buffer.clear();
                    document_sorter_buffer.push(Operation::Addition as u8);
                    into_del_add_obkv(
                        KvReaderU16::new(base_obkv),
                        true,
                        keep_original_version,
                        &mut document_sorter_buffer,
                    )?;
                    self.original_sorter.insert(docid.to_be_bytes(), &document_sorter_buffer)?;
                    match self.flatten_from_fields_ids_map(KvReader::new(base_obkv))? {
                        Some(flattened_obkv) => {
                            // we recreate our buffer with the flattened documents
                            document_sorter_buffer.clear();
                            document_sorter_buffer.push(Operation::Addition as u8);
                            into_del_add_obkv(
                                KvReaderU16::new(&flattened_obkv),
                                true,
                                keep_original_version,
                                &mut document_sorter_buffer,
                            )?;
                            self.flattened_sorter
                                .insert(docid.to_be_bytes(), &document_sorter_buffer)?
                        }
                        None => self
                            .flattened_sorter
                            .insert(docid.to_be_bytes(), &document_sorter_buffer)?,
                    }
                }
            }

            if !skip_insertion {
                self.new_documents_ids.insert(docid);

                document_sorter_buffer.clear();
                document_sorter_buffer.push(Operation::Addition as u8);
                into_del_add_obkv(
                    KvReaderU16::new(&obkv_buffer),
                    false,
                    true,
                    &mut document_sorter_buffer,
                )?;
                // We use the extracted/generated user id as the key for this document.
                self.original_sorter.insert(docid.to_be_bytes(), &document_sorter_buffer)?;

                match self.flatten_from_fields_ids_map(KvReader::new(&obkv_buffer))? {
                    Some(flattened_obkv) => {
                        document_sorter_buffer.clear();
                        document_sorter_buffer.push(Operation::Addition as u8);
                        into_del_add_obkv(
                            KvReaderU16::new(&flattened_obkv),
                            false,
                            true,
                            &mut document_sorter_buffer,
                        )?;
                        self.flattened_sorter
                            .insert(docid.to_be_bytes(), &document_sorter_buffer)?
                    }
                    None => self
                        .flattened_sorter
                        .insert(docid.to_be_bytes(), &document_sorter_buffer)?,
                }
            }
            documents_count += 1;

            progress_callback(UpdateIndexingStep::RemapDocumentAddition {
                documents_seen: documents_count,
            });

            field_buffer = drop_and_reuse(field_buffer_cache);
            docid_buffer.clear();
            obkv_buffer.clear();
        }

        progress_callback(UpdateIndexingStep::RemapDocumentAddition {
            documents_seen: documents_count,
        });

        self.index.put_fields_ids_map(wtxn, &self.fields_ids_map)?;
        self.index.put_primary_key(wtxn, &primary_key)?;
        self.documents_count += documents_count;
        // Now that we have a valid sorter that contains the user id and the obkv we
        // give it to the last transforming function which returns the TransformOutput.
        Ok(documents_count)
    }

    /// The counter part of `read_documents` that removes documents either from the transform or the database.
    /// It can be called before, after or in between two calls of the `read_documents`.
    ///
    /// It needs to update all the internal datastructure in the transform.
    /// - If the document is coming from the database -> it's marked as a to_delete document
    /// - If the document to remove was inserted by the `read_documents` method before AND was present in the db,
    ///   it's marked as `to_delete` + added into the grenad to ensure we don't reinsert it.
    /// - If the document to remove was inserted by the `read_documents` method before but was NOT present in the db,
    ///   it's added into the grenad to ensure we don't insert it + removed from the list of new documents ids.
    /// - If the document to remove was not present in either the db or the transform we do nothing.
    pub fn remove_documents<FA>(
        &mut self,
        mut to_remove: Vec<String>,
        wtxn: &mut heed::RwTxn,
        should_abort: FA,
    ) -> Result<usize>
    where
        FA: Fn() -> bool + Sync,
    {
        // there may be duplicates in the documents to remove.
        to_remove.sort_unstable();
        to_remove.dedup();

        let external_documents_ids = self.index.external_documents_ids(wtxn)?;

        let mut documents_deleted = 0;
        let mut document_sorter_buffer = Vec::new();
        for to_remove in to_remove {
            if should_abort() {
                return Err(Error::InternalError(InternalError::AbortedIndexation));
            }

            // Check if the document has been added in the current indexing process.
            let deleted_from_current = match self
                .new_external_documents_ids_builder
                .entry((*to_remove).into())
            {
                // if the document was added in a previous iteration of the transform we make it as deleted in the sorters.
                Entry::Occupied(entry) => {
                    let doc_id = *entry.get() as u32;
                    document_sorter_buffer.clear();
                    document_sorter_buffer.push(Operation::Deletion as u8);
                    obkv::KvWriterU16::new(&mut document_sorter_buffer).finish().unwrap();
                    self.original_sorter.insert(doc_id.to_be_bytes(), &document_sorter_buffer)?;
                    self.flattened_sorter.insert(doc_id.to_be_bytes(), &document_sorter_buffer)?;

                    // we must NOT update the list of replaced_documents_ids
                    // Either:
                    // 1. It's already in it and there is nothing to do
                    // 2. It wasn't in it because the document was created by a previous batch and since
                    //    we're removing it there is nothing to do.
                    self.new_documents_ids.remove(doc_id);
                    entry.remove_entry();
                    true
                }
                Entry::Vacant(_) => false,
            };

            // If the document was already in the db we mark it as a `to_delete` document.
            // Then we push the document in sorters in deletion mode.
            let deleted_from_db = match external_documents_ids.get(&to_remove) {
                Some(docid) => {
                    self.replaced_documents_ids.insert(docid);

                    // fetch the obkv document
                    let original_key = BEU32::new(docid);
                    let base_obkv = self
                        .index
                        .documents
                        .remap_data_type::<heed::types::ByteSlice>()
                        .get(wtxn, &original_key)?
                        .ok_or(InternalError::DatabaseMissingEntry {
                            db_name: db_name::DOCUMENTS,
                            key: None,
                        })?;

                    // push it as to delete in the original_sorter
                    document_sorter_buffer.clear();
                    document_sorter_buffer.push(Operation::Deletion as u8);
                    into_del_add_obkv(
                        KvReaderU16::new(base_obkv),
                        true,
                        false,
                        &mut document_sorter_buffer,
                    )?;
                    self.original_sorter.insert(docid.to_be_bytes(), &document_sorter_buffer)?;

                    // flatten it and push it as to delete in the flattened_sorter
                    match self.flatten_from_fields_ids_map(KvReader::new(base_obkv))? {
                        Some(flattened_obkv) => {
                            // we recreate our buffer with the flattened documents
                            document_sorter_buffer.clear();
                            document_sorter_buffer.push(Operation::Deletion as u8);
                            into_del_add_obkv(
                                KvReaderU16::new(&flattened_obkv),
                                true,
                                false,
                                &mut document_sorter_buffer,
                            )?;
                            self.flattened_sorter
                                .insert(docid.to_be_bytes(), &document_sorter_buffer)?
                        }
                        None => self
                            .flattened_sorter
                            .insert(docid.to_be_bytes(), &document_sorter_buffer)?,
                    }

                    true
                }
                None => false,
            };

            // increase counter only if the document existed somewhere before.
            if deleted_from_current || deleted_from_db {
                documents_deleted += 1;
            }
        }

        Ok(documents_deleted)
    }

    // Flatten a document from the fields ids map contained in self and insert the new
    // created fields. Returns `None` if the document doesn't need to be flattened.
    fn flatten_from_fields_ids_map(&mut self, obkv: KvReader<FieldId>) -> Result<Option<Vec<u8>>> {
        if obkv
            .iter()
            .all(|(_, value)| !json_depth_checker::should_flatten_from_unchecked_slice(value))
        {
            return Ok(None);
        }

        // store the keys and values the original obkv + the flattened json
        // We first extract all the key+value out of the obkv. If a value is not nested
        // we keep a reference on its value. If the value is nested we'll get its value
        // as an owned `Vec<u8>` after flattening it.
        let mut key_value: Vec<(FieldId, Cow<[u8]>)> = Vec::new();

        // the object we're going to use to store the fields that need to be flattened.
        let mut doc = serde_json::Map::new();

        // we recreate a json containing only the fields that needs to be flattened.
        // all the raw values get inserted directly in the `key_value` vec.
        for (key, value) in obkv.iter() {
            if json_depth_checker::should_flatten_from_unchecked_slice(value) {
                let key = self.fields_ids_map.name(key).ok_or(FieldIdMapMissingEntry::FieldId {
                    field_id: key,
                    process: "Flatten from fields ids map.",
                })?;

                let value = serde_json::from_slice::<Value>(value)
                    .map_err(crate::error::InternalError::SerdeJson)?;
                doc.insert(key.to_string(), value);
            } else {
                key_value.push((key, value.into()));
            }
        }

        let flattened = flatten_serde_json::flatten(&doc);

        // Once we have the flattened version we insert all the new generated fields_ids
        // (if any) in the fields ids map and serialize the value.
        for (key, value) in flattened.into_iter() {
            let fid = self.fields_ids_map.insert(&key).ok_or(UserError::AttributeLimitReached)?;
            let value = serde_json::to_vec(&value).map_err(InternalError::SerdeJson)?;
            key_value.push((fid, value.into()));
        }

        // we sort the key. If there was a conflict between the obkv and the new generated value the
        // keys will be consecutive.
        key_value.sort_unstable_by_key(|(key, _)| *key);

        let mut buffer = Vec::new();
        Self::create_obkv_from_key_value(&mut key_value, &mut buffer)?;
        Ok(Some(buffer))
    }

    /// Generate an obkv from a slice of key / value sorted by key.
    fn create_obkv_from_key_value(
        key_value: &mut [(FieldId, Cow<[u8]>)],
        output_buffer: &mut Vec<u8>,
    ) -> Result<()> {
        debug_assert!(
            key_value.windows(2).all(|vec| vec[0].0 <= vec[1].0),
            "The slice of key / value pair must be sorted."
        );

        output_buffer.clear();
        let mut writer = KvWriter::new(output_buffer);

        let mut skip_next_value = false;
        for things in key_value.windows(2) {
            if skip_next_value {
                skip_next_value = false;
                continue;
            }
            let (key1, value1) = &things[0];
            let (key2, value2) = &things[1];

            // now we're going to look for conflicts between the keys. For example the following documents would cause a conflict:
            // { "doggo.name": "jean", "doggo": { "name": "paul" } }
            // we should find a first "doggo.name" from the obkv and a second one from the flattening.
            // but we must generate the following document:
            // { "doggo.name": ["jean", "paul"] }
            // thus we're going to merge the value from the obkv and the flattened document in a single array and skip the next
            // iteration.
            if key1 == key2 {
                skip_next_value = true;

                let value1 = serde_json::from_slice(value1)
                    .map_err(crate::error::InternalError::SerdeJson)?;
                let value2 = serde_json::from_slice(value2)
                    .map_err(crate::error::InternalError::SerdeJson)?;
                let value = match (value1, value2) {
                    (Value::Array(mut left), Value::Array(mut right)) => {
                        left.append(&mut right);
                        Value::Array(left)
                    }
                    (Value::Array(mut array), value) | (value, Value::Array(mut array)) => {
                        array.push(value);
                        Value::Array(array)
                    }
                    (left, right) => Value::Array(vec![left, right]),
                };

                let value = serde_json::to_vec(&value).map_err(InternalError::SerdeJson)?;
                writer.insert(*key1, value)?;
            } else {
                writer.insert(*key1, value1)?;
            }
        }

        if !skip_next_value {
            // the unwrap is safe here, we know there was at least one value in the document
            let (key, value) = key_value.last().unwrap();
            writer.insert(*key, value)?;
        }

        Ok(())
    }

    fn remove_deleted_documents_from_field_distribution(
        &self,
        rtxn: &RoTxn,
        field_distribution: &mut FieldDistribution,
    ) -> Result<()> {
        for deleted_docid in self.replaced_documents_ids.iter() {
            let obkv = self.index.documents.get(rtxn, &BEU32::new(deleted_docid))?.ok_or(
                InternalError::DatabaseMissingEntry { db_name: db_name::DOCUMENTS, key: None },
            )?;

            for (key, _) in obkv.iter() {
                let name =
                    self.fields_ids_map.name(key).ok_or(FieldIdMapMissingEntry::FieldId {
                        field_id: key,
                        process: "Computing field distribution in transform.",
                    })?;
                // We checked that the document was in the db earlier. If we can't find it it means
                // there is an inconsistency between the field distribution and the field id map.
                let field =
                    field_distribution.get_mut(name).ok_or(FieldIdMapMissingEntry::FieldId {
                        field_id: key,
                        process: "Accessing field distribution in transform.",
                    })?;
                *field -= 1;
                if *field == 0 {
                    // since we were able to get the field right before it's safe to unwrap here
                    field_distribution.remove(name).unwrap();
                }
            }
        }
        Ok(())
    }

    /// Generate the `TransformOutput` based on the given sorter that can be generated from any
    /// format like CSV, JSON or JSON stream. This sorter must contain a key that is the document
    /// id for the user side and the value must be an obkv where keys are valid fields ids.
    pub(crate) fn output_from_sorter<F>(
        self,
        wtxn: &mut heed::RwTxn,
        progress_callback: F,
    ) -> Result<TransformOutput>
    where
        F: Fn(UpdateIndexingStep) + Sync,
    {
        puffin::profile_function!();

        let primary_key = self
            .index
            .primary_key(wtxn)?
            .ok_or(Error::InternalError(InternalError::DatabaseMissingEntry {
                db_name: db_name::MAIN,
                key: Some(main_key::PRIMARY_KEY_KEY),
            }))?
            .to_string();

        // We create a final writer to write the new documents in order from the sorter.
        let mut writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        // To compute the field distribution we need to;
        // 1. Remove all the deleted documents from the field distribution
        // 2. Add all the new documents to the field distribution
        let mut field_distribution = self.index.field_distribution(wtxn)?;

        self.remove_deleted_documents_from_field_distribution(wtxn, &mut field_distribution)?;

        // Here we are going to do the document count + field distribution + `write_into_stream_writer`
        let mut iter = self.original_sorter.into_stream_merger_iter()?;
        // used only for the callback
        let mut documents_count = 0;

        while let Some((key, val)) = iter.next()? {
            // skip first byte corresponding to the operation type (Deletion or Addition).
            let val = &val[1..];

            // send a callback to show at which step we are
            documents_count += 1;
            progress_callback(UpdateIndexingStep::ComputeIdsAndMergeDocuments {
                documents_seen: documents_count,
                total_documents: self.documents_count,
            });

            // We increment all the field of the current document in the field distribution.
            let obkv = KvReader::new(val);

            for (key, _) in obkv.iter() {
                let name =
                    self.fields_ids_map.name(key).ok_or(FieldIdMapMissingEntry::FieldId {
                        field_id: key,
                        process: "Computing field distribution in transform.",
                    })?;
                *field_distribution.entry(name.to_string()).or_insert(0) += 1;
            }
            writer.insert(key, val)?;
        }

        let mut original_documents = writer.into_inner()?;
        // We then extract the file and reset the seek to be able to read it again.
        original_documents.rewind()?;

        // We create a final writer to write the new documents in order from the sorter.
        let mut writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        // Once we have written all the documents into the final sorter, we write the nested documents
        // into this writer.
        // We get rids of the `Operation` byte and skip the deleted documents as well.
        let mut iter = self.flattened_sorter.into_stream_merger_iter()?;
        while let Some((key, val)) = iter.next()? {
            // skip first byte corresponding to the operation type (Deletion or Addition).
            let val = &val[1..];
            writer.insert(key, val)?;
        }
        let mut flattened_documents = writer.into_inner()?;
        flattened_documents.rewind()?;

        let mut new_external_documents_ids_builder: Vec<_> =
            self.new_external_documents_ids_builder.into_iter().collect();

        new_external_documents_ids_builder
            .sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let mut fst_new_external_documents_ids_builder = fst::MapBuilder::memory();
        new_external_documents_ids_builder.into_iter().try_for_each(|(key, value)| {
            fst_new_external_documents_ids_builder.insert(key, value)
        })?;
        let new_external_documents_ids = fst_new_external_documents_ids_builder.into_map();

        Ok(TransformOutput {
            primary_key,
            fields_ids_map: self.fields_ids_map,
            field_distribution,
            new_external_documents_ids: new_external_documents_ids.map_data(Cow::Owned).unwrap(),
            new_documents_ids: self.new_documents_ids,
            replaced_documents_ids: self.replaced_documents_ids,
            documents_count: self.documents_count,
            original_documents: original_documents.into_inner().map_err(|err| err.into_error())?,
            flattened_documents: flattened_documents
                .into_inner()
                .map_err(|err| err.into_error())?,
        })
    }

    /// Clear all databases. Returns a `TransformOutput` with a file that contains the documents
    /// of the index with the attributes reordered accordingly to the `FieldsIdsMap` given as argument.
    ///
    // TODO this can be done in parallel by using the rayon `ThreadPool`.
    pub fn prepare_for_documents_reindexing(
        self,
        wtxn: &mut heed::RwTxn<'i, '_>,
        old_fields_ids_map: FieldsIdsMap,
        mut new_fields_ids_map: FieldsIdsMap,
    ) -> Result<TransformOutput> {
        // There already has been a document addition, the primary key should be set by now.
        let primary_key = self
            .index
            .primary_key(wtxn)?
            .ok_or(InternalError::DatabaseMissingEntry {
                db_name: db_name::MAIN,
                key: Some(main_key::PRIMARY_KEY_KEY),
            })?
            .to_string();
        let field_distribution = self.index.field_distribution(wtxn)?;

        let documents_ids = self.index.documents_ids(wtxn)?;
        let documents_count = documents_ids.len() as usize;

        // We create a final writer to write the new documents in order from the sorter.
        let mut original_writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        // We create a final writer to write the new documents in order from the sorter.
        let mut flattened_writer = create_writer(
            self.indexer_settings.chunk_compression_type,
            self.indexer_settings.chunk_compression_level,
            tempfile::tempfile()?,
        );

        let mut obkv_buffer = Vec::new();
        let mut document_sorter_buffer = Vec::new();
        for result in self.index.all_documents(wtxn)? {
            let (docid, obkv) = result?;

            obkv_buffer.clear();
            let mut obkv_writer = obkv::KvWriter::<_, FieldId>::new(&mut obkv_buffer);

            // We iterate over the new `FieldsIdsMap` ids in order and construct the new obkv.
            for (id, name) in new_fields_ids_map.iter() {
                if let Some(val) = old_fields_ids_map.id(name).and_then(|id| obkv.get(id)) {
                    obkv_writer.insert(id, val)?;
                }
            }

            let buffer = obkv_writer.into_inner()?;
            document_sorter_buffer.clear();
            into_del_add_obkv(KvReaderU16::new(buffer), false, true, &mut document_sorter_buffer)?;
            original_writer.insert(docid.to_be_bytes(), &document_sorter_buffer)?;

            // Once we have the document. We're going to flatten it
            // and insert it in the flattened sorter.
            let mut doc = serde_json::Map::new();

            let reader = obkv::KvReader::new(buffer);
            for (k, v) in reader.iter() {
                let key = new_fields_ids_map.name(k).ok_or(FieldIdMapMissingEntry::FieldId {
                    field_id: k,
                    process: "Accessing field distribution in transform.",
                })?;
                let value = serde_json::from_slice::<serde_json::Value>(v)
                    .map_err(InternalError::SerdeJson)?;
                doc.insert(key.to_string(), value);
            }

            let flattened = flatten_serde_json::flatten(&doc);

            // Once we have the flattened version we can convert it back to obkv and
            // insert all the new generated fields_ids (if any) in the fields ids map.
            let mut buffer: Vec<u8> = Vec::new();
            let mut writer = KvWriter::new(&mut buffer);
            let mut flattened: Vec<_> = flattened.into_iter().collect();
            // we reorder the field to get all the known field first
            flattened.sort_unstable_by_key(|(key, _)| {
                new_fields_ids_map.id(key).unwrap_or(FieldId::MAX)
            });

            for (key, value) in flattened {
                let fid =
                    new_fields_ids_map.insert(&key).ok_or(UserError::AttributeLimitReached)?;
                let value = serde_json::to_vec(&value).map_err(InternalError::SerdeJson)?;
                writer.insert(fid, &value)?;
            }
            document_sorter_buffer.clear();
            into_del_add_obkv(KvReaderU16::new(&buffer), false, true, &mut document_sorter_buffer)?;
            flattened_writer.insert(docid.to_be_bytes(), &document_sorter_buffer)?;
        }

        // Once we have written all the documents, we extract
        // the file and reset the seek to be able to read it again.
        let mut original_documents = original_writer.into_inner()?;
        original_documents.rewind()?;

        let mut flattened_documents = flattened_writer.into_inner()?;
        flattened_documents.rewind()?;

        let output = TransformOutput {
            primary_key,
            fields_ids_map: new_fields_ids_map,
            field_distribution,
            // FIXME: remove this now unused field
            new_external_documents_ids: fst::Map::default().map_data(Cow::Owned).unwrap(),
            new_documents_ids: documents_ids,
            // FIXME: remove this now unused field
            replaced_documents_ids: RoaringBitmap::default(),
            documents_count,
            original_documents: original_documents.into_inner().map_err(|err| err.into_error())?,
            flattened_documents: flattened_documents
                .into_inner()
                .map_err(|err| err.into_error())?,
        };

        let new_facets = output.compute_real_facets(wtxn, self.index)?;
        self.index.put_faceted_fields(wtxn, &new_facets)?;

        // We clear the full database (words-fst, documents ids and documents content).
        ClearDocuments::new(wtxn, self.index).execute()?;

        Ok(output)
    }
}

/// Drops all the value of type `U` in vec, and reuses the allocation to create a `Vec<T>`.
///
/// The size and alignment of T and U must match.
fn drop_and_reuse<U, T>(mut vec: Vec<U>) -> Vec<T> {
    debug_assert_eq!(std::mem::align_of::<U>(), std::mem::align_of::<T>());
    debug_assert_eq!(std::mem::size_of::<U>(), std::mem::size_of::<T>());
    vec.clear();
    debug_assert!(vec.is_empty());
    vec.into_iter().map(|_| unreachable!()).collect()
}

impl TransformOutput {
    // find and insert the new field ids
    pub fn compute_real_facets(&self, rtxn: &RoTxn, index: &Index) -> Result<HashSet<String>> {
        let user_defined_facets = index.user_defined_faceted_fields(rtxn)?;

        Ok(self
            .fields_ids_map
            .names()
            .filter(|&field| crate::is_faceted(field, &user_defined_facets))
            .map(|field| field.to_string())
            .collect())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn merge_obkvs() {
        let mut additive_doc_0 = Vec::new();
        let mut deletive_doc_0 = Vec::new();
        let mut del_add_doc_0 = Vec::new();
        let mut kv_writer = KvWriter::memory();
        kv_writer.insert(0_u8, [0]).unwrap();
        let buffer = kv_writer.into_inner().unwrap();
        into_del_add_obkv(KvReaderU16::new(&buffer), false, true, &mut additive_doc_0).unwrap();
        additive_doc_0.insert(0, Operation::Addition as u8);
        into_del_add_obkv(KvReaderU16::new(&buffer), true, false, &mut deletive_doc_0).unwrap();
        deletive_doc_0.insert(0, Operation::Deletion as u8);
        into_del_add_obkv(KvReaderU16::new(&buffer), true, true, &mut del_add_doc_0).unwrap();
        del_add_doc_0.insert(0, Operation::Addition as u8);

        let mut additive_doc_1 = Vec::new();
        let mut kv_writer = KvWriter::memory();
        kv_writer.insert(1_u8, [1]).unwrap();
        let buffer = kv_writer.into_inner().unwrap();
        into_del_add_obkv(KvReaderU16::new(&buffer), false, true, &mut additive_doc_1).unwrap();
        additive_doc_1.insert(0, Operation::Addition as u8);

        let mut additive_doc_0_1 = Vec::new();
        let mut kv_writer = KvWriter::memory();
        kv_writer.insert(0_u8, [0]).unwrap();
        kv_writer.insert(1_u8, [1]).unwrap();
        let buffer = kv_writer.into_inner().unwrap();
        into_del_add_obkv(KvReaderU16::new(&buffer), false, true, &mut additive_doc_0_1).unwrap();
        additive_doc_0_1.insert(0, Operation::Addition as u8);

        let ret = obkvs_merge_additions_and_deletions(&[], &[Cow::from(additive_doc_0.as_slice())])
            .unwrap();
        assert_eq!(*ret, additive_doc_0);

        let ret = obkvs_merge_additions_and_deletions(
            &[],
            &[Cow::from(deletive_doc_0.as_slice()), Cow::from(additive_doc_0.as_slice())],
        )
        .unwrap();
        assert_eq!(*ret, del_add_doc_0);

        let ret = obkvs_merge_additions_and_deletions(
            &[],
            &[Cow::from(additive_doc_0.as_slice()), Cow::from(deletive_doc_0.as_slice())],
        )
        .unwrap();
        assert_eq!(*ret, deletive_doc_0);

        let ret = obkvs_merge_additions_and_deletions(
            &[],
            &[
                Cow::from(additive_doc_1.as_slice()),
                Cow::from(deletive_doc_0.as_slice()),
                Cow::from(additive_doc_0.as_slice()),
            ],
        )
        .unwrap();
        assert_eq!(*ret, del_add_doc_0);

        let ret = obkvs_merge_additions_and_deletions(
            &[],
            &[Cow::from(additive_doc_1.as_slice()), Cow::from(additive_doc_0.as_slice())],
        )
        .unwrap();
        assert_eq!(*ret, additive_doc_0_1);

        let ret = obkvs_keep_last_addition_merge_deletions(
            &[],
            &[Cow::from(additive_doc_1.as_slice()), Cow::from(additive_doc_0.as_slice())],
        )
        .unwrap();
        assert_eq!(*ret, additive_doc_0);

        let ret = obkvs_keep_last_addition_merge_deletions(
            &[],
            &[
                Cow::from(deletive_doc_0.as_slice()),
                Cow::from(additive_doc_1.as_slice()),
                Cow::from(additive_doc_0.as_slice()),
            ],
        )
        .unwrap();
        assert_eq!(*ret, del_add_doc_0);
    }
}
