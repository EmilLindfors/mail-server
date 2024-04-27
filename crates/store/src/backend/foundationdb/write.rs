/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::{
    cmp::Ordering,
    time::{Duration, Instant},
};

use foundationdb::{
    options::{self, MutationType, StreamingMode},
    FdbError, KeySelector, RangeOption,
};
use futures::StreamExt;
use rand::Rng;
use roaring::RoaringBitmap;

use crate::{
    backend::deserialize_i64_le,
    write::{
        key::{DeserializeBigEndian, KeySerializer},
        AssignedIds, Batch, BitmapClass, Operation, RandomAvailableId, ValueOp,
        MAX_COMMIT_ATTEMPTS, MAX_COMMIT_TIME,
    },
    BitmapKey, IndexKey, Key, LogKey, SUBSPACE_COUNTER, SUBSPACE_QUOTA, U32_LEN, WITH_SUBSPACE,
};

use super::{
    read::{read_chunked_value, ChunkedValue},
    FdbStore, MAX_VALUE_SIZE,
};

impl FdbStore {
    pub(crate) async fn write(&self, batch: Batch) -> crate::Result<AssignedIds> {
        let start = Instant::now();
        let mut retry_count = 0;

        loop {
            let mut account_id = u32::MAX;
            let mut collection = u8::MAX;
            let mut document_id = u32::MAX;
            let mut result = AssignedIds::default();

            let trx = self.db.create_trx()?;

            for op in &batch.ops {
                match op {
                    Operation::AccountId {
                        account_id: account_id_,
                    } => {
                        account_id = *account_id_;
                    }
                    Operation::Collection {
                        collection: collection_,
                    } => {
                        collection = *collection_;
                    }
                    Operation::DocumentId {
                        document_id: document_id_,
                    } => {
                        document_id = *document_id_;
                    }
                    Operation::Value { class, op } => {
                        let mut key = class.serialize(
                            account_id,
                            collection,
                            document_id,
                            WITH_SUBSPACE,
                            (&result).into(),
                        );
                        let do_chunk = !class.is_counter(collection);

                        match op {
                            ValueOp::Set(value) => {
                                let value = value.resolve(&result)?;
                                if !value.is_empty() && do_chunk {
                                    for (pos, chunk) in value.chunks(MAX_VALUE_SIZE).enumerate() {
                                        match pos.cmp(&1) {
                                            Ordering::Less => {}
                                            Ordering::Equal => {
                                                key.push(0);
                                            }
                                            Ordering::Greater => {
                                                if pos < u8::MAX as usize {
                                                    *key.last_mut().unwrap() += 1;
                                                } else {
                                                    trx.cancel();
                                                    return Err(crate::Error::InternalError(
                                                        "Value too large".into(),
                                                    ));
                                                }
                                            }
                                        }
                                        trx.set(&key, chunk);
                                    }
                                } else {
                                    trx.set(&key, value.as_ref());
                                }
                            }
                            ValueOp::AtomicAdd(by) => {
                                trx.atomic_op(&key, &by.to_le_bytes()[..], MutationType::Add);
                            }
                            ValueOp::AddAndGet(by) => {
                                let num = if let Some(bytes) = trx.get(&key, false).await? {
                                    deserialize_i64_le(&bytes)? + *by
                                } else {
                                    *by
                                };
                                trx.set(&key, &num.to_le_bytes()[..]);
                                result.push_counter_id(num);
                            }
                            ValueOp::Clear => {
                                if do_chunk {
                                    trx.clear_range(
                                        &key,
                                        &KeySerializer::new(key.len() + 1)
                                            .write(key.as_slice())
                                            .write(u8::MAX)
                                            .finalize(),
                                    );
                                } else {
                                    trx.clear(&key);
                                }
                            }
                        }
                    }
                    Operation::Index { field, key, set } => {
                        let key = IndexKey {
                            account_id,
                            collection,
                            document_id,
                            field: *field,
                            key,
                        }
                        .serialize(WITH_SUBSPACE);

                        if *set {
                            trx.set(&key, &[]);
                        } else {
                            trx.clear(&key);
                        }
                    }
                    Operation::Bitmap { class, set } => {
                        // Find the next available document id
                        let assign_id = *set
                            && matches!(class, BitmapClass::DocumentIds)
                            && document_id == u32::MAX;
                        if assign_id {
                            let begin = BitmapKey {
                                account_id,
                                collection,
                                class: BitmapClass::DocumentIds,
                                document_id: 0,
                            }
                            .serialize(WITH_SUBSPACE);
                            let end = BitmapKey {
                                account_id,
                                collection,
                                class: BitmapClass::DocumentIds,
                                document_id: u32::MAX,
                            }
                            .serialize(WITH_SUBSPACE);
                            let key_len = begin.len();
                            let mut values = trx.get_ranges(
                                RangeOption {
                                    begin: KeySelector::first_greater_or_equal(begin),
                                    end: KeySelector::first_greater_or_equal(end),
                                    mode: StreamingMode::WantAll,
                                    reverse: false,
                                    ..RangeOption::default()
                                },
                                true,
                            );
                            let mut found_ids = RoaringBitmap::new();
                            while let Some(values) = values.next().await {
                                for value in values? {
                                    let key = value.key();
                                    if key.len() == key_len {
                                        found_ids
                                            .insert(key.deserialize_be_u32(key_len - U32_LEN)?);
                                    } else {
                                        break;
                                    }
                                }
                            }
                            document_id = found_ids.random_available_id();
                            result.push_document_id(document_id);
                        }

                        let key = class.serialize(
                            account_id,
                            collection,
                            document_id,
                            WITH_SUBSPACE,
                            (&result).into(),
                        );

                        if *set {
                            if assign_id {
                                trx.add_conflict_range(
                                    &key,
                                    &class.serialize(
                                        account_id,
                                        collection,
                                        document_id + 1,
                                        WITH_SUBSPACE,
                                        (&result).into(),
                                    ),
                                    options::ConflictRangeType::Read,
                                )?;
                            }

                            trx.set(&key, &[]);
                        } else {
                            trx.clear(&key);
                        }
                    }
                    Operation::Log { set } => {
                        let key = LogKey {
                            account_id,
                            collection,
                            change_id: batch.change_id,
                        }
                        .serialize(WITH_SUBSPACE);
                        trx.set(&key, set.resolve(&result)?.as_ref());
                    }
                    Operation::AssertValue {
                        class,
                        assert_value,
                    } => {
                        let key = class.serialize(
                            account_id,
                            collection,
                            document_id,
                            WITH_SUBSPACE,
                            (&result).into(),
                        );

                        let matches = match read_chunked_value(&key, &trx, false).await {
                            Ok(ChunkedValue::Single(bytes)) => assert_value.matches(bytes.as_ref()),
                            Ok(ChunkedValue::Chunked { bytes, .. }) => {
                                assert_value.matches(bytes.as_ref())
                            }
                            Ok(ChunkedValue::None) => assert_value.is_none(),
                            Err(_) => false,
                        };

                        if !matches {
                            trx.cancel();
                            return Err(crate::Error::AssertValueFailed);
                        }
                    }
                }
            }

            match trx.commit().await {
                Ok(_) => {
                    return Ok(result);
                }
                Err(err) => {
                    if retry_count < MAX_COMMIT_ATTEMPTS && start.elapsed() < MAX_COMMIT_TIME {
                        err.on_error().await?;
                        let backoff = rand::thread_rng().gen_range(50..=300);
                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                        retry_count += 1;
                    } else {
                        return Err(FdbError::from(err).into());
                    }
                }
            }
        }
    }

    pub(crate) async fn purge_store(&self) -> crate::Result<()> {
        // Obtain all zero counters
        let mut delete_keys = Vec::new();
        for subspace in [SUBSPACE_COUNTER, SUBSPACE_QUOTA] {
            let trx = self.db.create_trx()?;
            let from_key = [subspace, 0u8];
            let to_key = [subspace, u8::MAX, u8::MAX, u8::MAX, u8::MAX, u8::MAX];

            let mut iter = trx.get_ranges(
                RangeOption {
                    begin: KeySelector::first_greater_or_equal(&from_key[..]),
                    end: KeySelector::first_greater_or_equal(&to_key[..]),
                    mode: options::StreamingMode::WantAll,
                    reverse: false,
                    ..Default::default()
                },
                true,
            );

            while let Some(values) = iter.next().await {
                for value in values? {
                    if value.value().iter().all(|byte| *byte == 0) {
                        delete_keys.push(value.key().to_vec());
                    }
                }
            }
        }

        if delete_keys.is_empty() {
            return Ok(());
        }

        // Delete keys
        let integer = 0i64.to_le_bytes();
        for chunk in delete_keys.chunks(1024) {
            let mut retry_count = 0;
            loop {
                let trx = self.db.create_trx()?;
                for key in chunk {
                    trx.atomic_op(key, &integer, MutationType::CompareAndClear);
                }
                match trx.commit().await {
                    Ok(_) => {
                        break;
                    }
                    Err(err) => {
                        if retry_count < MAX_COMMIT_ATTEMPTS {
                            err.on_error().await?;
                            retry_count += 1;
                        } else {
                            return Err(FdbError::from(err).into());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn delete_range(&self, from: impl Key, to: impl Key) -> crate::Result<()> {
        let from = from.serialize(WITH_SUBSPACE);
        let to = to.serialize(WITH_SUBSPACE);

        let trx = self.db.create_trx()?;
        trx.clear_range(&from, &to);
        trx.commit()
            .await
            .map_err(|err| FdbError::from(err).into())
            .map(|_| ())
    }
}
