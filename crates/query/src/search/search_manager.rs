use crate::errors::QueryError;
use crate::managers::single::table_shard::TableShard;
use crate::ops::query_ops::{QueryOps, QueryVal};
use crate::row::Row;
use chashmap::CHashMap;
use schemajs_index::composite_key::CompositeKey;
use schemajs_primitives::index::Index;
use std::collections::HashSet;
use std::sync::Arc;

pub struct QuerySearchManager<T: Row<T>> {
    table_shards: Arc<CHashMap<String, TableShard<T>>>,
}

impl<T: Row<T>> QuerySearchManager<T> {
    pub fn new(table_shards: Arc<CHashMap<String, TableShard<T>>>) -> Self {
        Self { table_shards }
    }

    fn intersect_indices(a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
        let set_a: HashSet<u64> = a.into_iter().collect::<HashSet<u64>>();
        let set_b: HashSet<u64> = b.into_iter().collect::<HashSet<u64>>();
        set_a.intersection(&set_b).cloned().collect()
    }

    fn union_indices(a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
        let mut set_a: HashSet<u64> = a.into_iter().collect::<HashSet<u64>>();
        let set_b: HashSet<u64> = b.into_iter().collect::<HashSet<u64>>();
        set_a.extend(set_b);
        set_a.into_iter().collect()
    }

    fn get_index_for_condition(cond: &QueryVal, indexes: &Vec<Index>) -> Option<Index> {
        for index in indexes.iter() {
            if index.members.len() == 1 && index.members[0] == cond.key {
                return Some(index.clone());
            }
        }
        None
    }

    fn execute_query(&self, tbl: &TableShard<T>, query: &QueryOps) -> Vec<u64> {
        let indexes = &tbl.table.indexes;
        // Try to find an index that can be used for the entire query
        if let Some(index_query) = Self::find_index_for_query(query, indexes) {
            if let Some(indx_manager) = tbl.indexes.get(&index_query.0.name) {
                let manager = indx_manager.as_index();
                let key = manager.to_key(index_query.1);
                // TODO: get_all to return vec in index
                if let Some(pointer) = manager.get(&key) {
                    vec![pointer]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            // Evaluate recursively
            match query {
                QueryOps::Condition(cond) => {
                    return self.evaluate_condition(&tbl, cond, indexes);
                }
                QueryOps::And(ops) => {
                    let mut results: Option<Vec<u64>> = None;
                    for op in ops {
                        let res = self.execute_query(tbl, op);
                        results = match results {
                            Some(existing) => Some(Self::intersect_indices(existing, res)),
                            None => Some(res),
                        };
                    }
                    return results.unwrap_or_else(Vec::new);
                }
                QueryOps::Or(ops) => {
                    let mut results = Vec::new();
                    for op in ops {
                        let res = self.execute_query(tbl, op);
                        results = Self::union_indices(results, res);
                    }
                    return results;
                }
            }
        }
    }

    fn evaluate_condition(
        &self,
        shard: &TableShard<T>,
        cond: &QueryVal,
        indexes: &Vec<Index>,
    ) -> Vec<u64> {
        if cond.filter_type != "=" {
            // Only "=" is supported
            return Vec::new();
        }

        if let Some(index) = Self::get_index_for_condition(cond, indexes) {
            let comp_key = CompositeKey(vec![(cond.key.to_string(), (&cond.value).to_string())]);

            let indx_read = shard.indexes.get(&index.name).unwrap();
            let indx = indx_read.as_index();
            let key = indx.to_key(comp_key);
            if let Some(pointer) = indx.get(&key) {
                return vec![pointer];
            }
        }

        vec![]
    }

    fn find_index_for_query(
        query: &QueryOps,
        indexes: &Vec<Index>,
    ) -> Option<(Index, CompositeKey)> {
        if let Some(conditions) = Self::collect_conditions(query) {
            if let Some(index) = Self::find_index_for_conditions(&conditions, indexes) {
                let key = Self::generate_index_key(&index, &conditions);
                if let Some(key) = key {
                    return Some((index, key));
                }
            }
        }

        None
    }

    fn find_index_for_conditions(conditions: &[QueryVal], indexes: &Vec<Index>) -> Option<Index> {
        let condition_keys: HashSet<String> =
            conditions.iter().map(|cond| cond.key.clone()).collect();
        for index in indexes.iter() {
            let index_keys: HashSet<String> = index.members.iter().cloned().collect();
            if condition_keys.is_subset(&index_keys) {
                return Some(index.clone());
            }
        }
        None
    }

    fn collect_conditions(query: &QueryOps) -> Option<Vec<QueryVal>> {
        match query {
            QueryOps::Condition(cond) => Some(vec![cond.clone()]),
            QueryOps::And(ops) => {
                let mut conditions = Vec::new();
                for op in ops {
                    if let Some(mut child_conditions) = Self::collect_conditions(op) {
                        conditions.append(&mut child_conditions);
                    } else {
                        // Cannot collect conditions due to nested OR
                        return None;
                    }
                }
                Some(conditions)
            }
            QueryOps::Or(_) => None, // Cannot collect conditions under OR
        }
    }

    fn generate_index_key(index: &Index, conditions: &[QueryVal]) -> Option<CompositeKey> {
        let mut key_parts = Vec::new();
        for member in &index.members {
            if let Some(cond) = conditions.iter().find(|c| &c.key == member) {
                key_parts.push((cond.key.to_string(), (&cond.value).to_string()));
            } else {
                // Missing condition for index member
                return None;
            }
        }

        Some(CompositeKey(key_parts))
    }

    pub fn search(&self, table_name: String, ops: &QueryOps) -> Result<Vec<T>, QueryError> {
        let get_table_shard = self
            .table_shards
            .get(&table_name)
            .ok_or_else(|| QueryError::InvalidTable(table_name.clone()))?;

        let pointers = self.execute_query(&get_table_shard, ops);

        let mut results = vec![];

        for pointer in pointers {
            let tbl_data = get_table_shard.data.read().unwrap();
            let data = tbl_data.get_element(pointer as usize).unwrap();
            results.push(T::from(&data))
        }

        Ok(results)
    }
}

#[cfg(test)]
mod test {
    use crate::managers::single::SingleQueryManager;
    use crate::ops::query_ops::{QueryOps, QueryVal};
    use crate::row::Row;
    use crate::row_json::{RowData, RowJson};
    use crate::search::search_manager::QuerySearchManager;
    use schemajs_dirs::create_scheme_js_db;
    use schemajs_index::index_type::IndexType;
    use schemajs_primitives::column::types::{DataTypes, DataValue};
    use schemajs_primitives::column::Column;
    use schemajs_primitives::index::Index;
    use schemajs_primitives::table::Table;
    use uuid::Uuid;

    #[flaky_test::flaky_test]
    pub fn test_search_manager() {
        let test_db = Uuid::new_v4().to_string();
        let db_folder = create_scheme_js_db(None, test_db.as_str());
        let query_manager = SingleQueryManager::new(test_db.clone());

        let tbl = Table::new("users")
            .add_column(Column::new("user_id", DataTypes::String))
            .add_column(Column::new("user_email", DataTypes::String))
            .add_column(Column::new("user_country", DataTypes::String))
            .add_column(Column::new("user_age", DataTypes::String))
            .add_column(Column::new("user_name", DataTypes::String))
            .add_index(Index {
                name: "user_id_indx".to_string(),
                members: vec![String::from("user_id")],
                index_type: IndexType::Hash,
            })
            .add_index(Index {
                name: "user_email_indx".to_string(),
                members: vec![String::from("user_email")],
                index_type: IndexType::Hash,
            })
            .add_index(Index {
                name: "user_country_indx".to_string(),
                members: vec![String::from("user_country")],
                index_type: IndexType::Hash,
            })
            .add_index(Index {
                name: "user_age_indx".to_string(),
                members: vec![String::from("user_age")],
                index_type: IndexType::Hash,
            })
            .add_index(Index {
                name: "user_name_indx".to_string(),
                members: vec![String::from("user_name")],
                index_type: IndexType::Hash,
            })
            .add_index(Index {
                name: "age_country_indx".to_string(),
                members: vec![String::from("user_age"), String::from("user_country")],
                index_type: IndexType::Hash,
            });

        query_manager.register_table(tbl);

        let row_1 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "0874d926-52a9-43e7-b682-9d7c5ec62b30",
                    "user_id": "1",
                    "user_email": "email@outlook.com",
                    "user_country": "US",
                    "user_age": "20",
                    "user_name": "andreespirela"
                }),
            }))
            .unwrap();

        let row_2 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "933a79e1-4d60-47b4-8f9d-2ee12ec75e37",
                    "user_id": "2",
                    "user_email": "email2@outlook.com",
                    "user_country": "US",
                    "user_age": "21",
                    "user_name": "Veronica"
                }),
            }))
            .unwrap();

        let row_3 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "968af9b6-c325-4c2a-ac35-b9f82429fcdf",
                    "user_id": "3",
                    "user_email": "email3@outlook.com",
                    "user_country": "US",
                    "user_age": "21",
                    "user_name": "superman"
                }),
            }))
            .unwrap();

        let row_4 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "c455eb4e-82ea-4974-bd74-0ea449c16d2c",
                    "user_id": "4",
                    "user_email": "email3@outlook.com",
                    "user_country": "US",
                    "user_age": "19",
                    "user_name": "Luis"
                }),
            }))
            .unwrap();

        let row_5 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "0977848d-18a9-49ec-a4e6-da51df3ae11d",
                    "user_id": "5",
                    "user_email": "email10@outlook.com",
                    "user_country": "US",
                    "user_age": "22",
                    "user_name": "Flash"
                }),
            }))
            .unwrap();

        let row_6 = query_manager
            .insert(RowJson::from(RowData {
                table: String::from("users"),
                value: serde_json::json!({
                    "_uid": "a44fbf77-7a62-46a0-ae81-c6f75048ab34",
                    "user_id": "6",
                    "user_email": "email10@outlook.com",
                    "user_country": "AR",
                    "user_age": "22",
                    "user_name": "Door"
                }),
            }))
            .unwrap();

        let tables = query_manager.tables.clone();
        let search_manager = QuerySearchManager::new(tables.clone());
        let ops = QueryOps::Or(vec![
            QueryOps::And(vec![
                QueryOps::Condition(QueryVal {
                    key: "user_age".to_string(),
                    filter_type: "=".to_string(),
                    value: DataValue::String("22".to_string()),
                }),
                QueryOps::Condition(QueryVal {
                    key: "user_country".to_string(),
                    filter_type: "=".to_string(),
                    value: DataValue::String("AR".to_string()),
                }),
            ]),
            QueryOps::Condition(QueryVal {
                key: "user_name".to_string(),
                filter_type: "=".to_string(),
                value: DataValue::String("Luis".to_string()),
            }),
        ]);

        let tbl = tables.get("users").unwrap();

        tbl.temps.reconcile_all();

        let results = search_manager.search("users".to_string(), &ops).unwrap();
        let row_0 = &results[0];

        let col = tbl.table.get_column("user_name").unwrap();

        assert_eq!(
            row_0.get_value(col).unwrap(),
            DataValue::String("Door".to_string())
        );
        let row_1 = &results[1];
        let res_name = row_1.get_value(col).unwrap();
        assert_eq!(res_name, DataValue::String("Luis".to_string()));

        println!("{}", res_name.to_string());
    }
}
