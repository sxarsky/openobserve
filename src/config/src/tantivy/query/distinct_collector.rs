// Copyright 2026 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashSet;

use tantivy::{
    DocId, Score, SegmentOrdinal, SegmentReader,
    collector::{Collector, SegmentCollector},
    columnar::StrColumn,
};

use super::topn_collector::resolve_ords;

/// Collects the distinct values of one string field over the documents matched
/// by the query, for `SELECT field ... GROUP BY field ORDER BY field LIMIT k`.
///
/// The query (which already carries the `_timestamp` range) drives a single
/// filtered pass; per matched doc we read the field's term ordinal — cheap and
/// columnar — and keep only the value-extreme `limit` distinct ordinals (string
/// ordinals are dictionary-sorted), resolving just those to strings. The result
/// is an unordered set; DataFusion re-sorts and re-applies the limit on top.
pub struct SimpleDistinctCollector {
    field: String,
    limit: usize,
    ascend: bool,
}

impl SimpleDistinctCollector {
    pub fn new(field: String, limit: usize, ascend: bool) -> Self {
        Self {
            field,
            limit,
            ascend,
        }
    }
}

pub struct SimpleDistinctSegmentCollector {
    /// `None` when the field's column is missing from this segment
    col: Option<StrColumn>,
    limit: usize,
    ascend: bool,
    ords: HashSet<u64>,
}

impl Collector for SimpleDistinctCollector {
    type Fruit = HashSet<String>;
    type Child = SimpleDistinctSegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(SimpleDistinctSegmentCollector {
            col: segment.fast_fields().str(&self.field)?,
            limit: self.limit,
            ascend: self.ascend,
            ords: HashSet::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, segment_fruits: Vec<HashSet<String>>) -> tantivy::Result<Self::Fruit> {
        // one parquet file is a single segment, but union for safety
        Ok(segment_fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for SimpleDistinctSegmentCollector {
    type Fruit = HashSet<String>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(col) = &self.col
            && let Some(ord) = col.ords().first(doc)
        {
            self.ords.insert(ord);
        }
    }

    fn harvest(self) -> Self::Fruit {
        let Some(col) = self.col else {
            return HashSet::new();
        };
        // string ordinals are dictionary-sorted, so the value-extreme `limit`
        // distinct values are the smallest (ascend) or largest (descend) ords
        let mut ords: Vec<u64> = self.ords.into_iter().collect();
        let selected = if ords.len() <= self.limit {
            ords
        } else if self.ascend {
            ords.select_nth_unstable(self.limit - 1);
            ords.truncate(self.limit);
            ords
        } else {
            let cut = ords.len() - self.limit;
            ords.select_nth_unstable(cut);
            ords.split_off(cut)
        };
        resolve_ords(&col, selected).into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, ops::Bound};

    use tantivy::{
        Index, Searcher, Term, doc,
        query::RangeQuery,
        schema::{FAST, IndexRecordOption, SchemaBuilder, TextFieldIndexing, TextOptions},
    };

    use super::*;

    /// Single-segment in-RAM index of `(timestamp, name)` rows, where `name` is
    /// a raw-tokenized fast text field and `_timestamp` is an i64 fast field.
    fn build_index(rows: &[(i64, &str)]) -> Searcher {
        let mut sb = SchemaBuilder::new();
        let ts = sb.add_i64_field("_timestamp", FAST);
        let name = sb.add_text_field(
            "name",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_index_option(IndexRecordOption::Basic)
                        .set_tokenizer("raw"),
                )
                .set_fast(None),
        );
        let index = Index::create_in_ram(sb.build());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for (timestamp, value) in rows {
            writer
                .add_document(doc!(ts => *timestamp, name => *value))
                .unwrap();
        }
        writer.commit().unwrap();
        index.reader().unwrap().searcher()
    }

    /// Runs the collector over docs with `_timestamp` in `[start, end)`.
    fn distinct(
        searcher: &Searcher,
        start: i64,
        end: i64,
        limit: usize,
        ascend: bool,
    ) -> HashSet<String> {
        let ts = searcher.schema().get_field("_timestamp").unwrap();
        let query = RangeQuery::new(
            Bound::Included(Term::from_field_i64(ts, start)),
            Bound::Excluded(Term::from_field_i64(ts, end)),
        );
        searcher
            .search(
                &query,
                &SimpleDistinctCollector::new("name".to_string(), limit, ascend),
            )
            .unwrap()
    }

    #[test]
    fn filters_by_time_range() {
        let s = build_index(&[(30, "c"), (20, "b"), (10, "a")]);
        // [15, 35) keeps b (20) and c (30), drops a (10)
        assert_eq!(
            distinct(&s, 15, 35, 10, false),
            HashSet::from(["b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn end_is_exclusive() {
        let s = build_index(&[(30, "c"), (20, "b"), (10, "a")]);
        // [10, 30): start inclusive keeps a (10), end exclusive drops c (30)
        assert_eq!(
            distinct(&s, 10, 30, 10, false),
            HashSet::from(["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn dedups_repeated_values() {
        // "b" appears both in and out of range (one in-range doc is enough),
        // "a" only out of range
        let s = build_index(&[(25, "b"), (20, "b"), (5, "a")]);
        assert_eq!(
            distinct(&s, 15, 30, 10, false),
            HashSet::from(["b".to_string()])
        );
    }

    #[test]
    fn limit_keeps_value_extreme() {
        let s = build_index(&[(50, "e"), (40, "d"), (30, "c"), (20, "b"), (10, "a")]);
        // ascend keeps the 2 smallest values, descend the 2 largest
        assert_eq!(
            distinct(&s, 0, 100, 2, true),
            HashSet::from(["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            distinct(&s, 0, 100, 2, false),
            HashSet::from(["d".to_string(), "e".to_string()])
        );
    }
}
