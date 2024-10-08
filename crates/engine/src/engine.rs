use crate::engine_db::EngineDb;
use crate::utils::fs::is_js_or_ts;
use anyhow::bail;
use deno_core::{ModuleId, ModuleSpecifier};
use schemajs_dirs::create_scheme_js_folder;
use schemajs_primitives::table::Table;
use std::future::Future;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct SchemeJsEngine {
    pub databases: Vec<EngineDb>,
    pub data_path_dir: Option<PathBuf>,
}

impl SchemeJsEngine {
    pub fn new(data_path: Option<PathBuf>) -> Self {
        create_scheme_js_folder(data_path.clone());

        Self {
            databases: vec![],
            data_path_dir: data_path,
        }
    }

    pub fn load_database_schema(
        &mut self,
        path: &PathBuf,
    ) -> anyhow::Result<(String, Vec<ModuleSpecifier>)> {
        if !path.exists() {
            bail!(
                "Trying to access a database schema that does not exist: {}",
                path.to_string_lossy()
            );
        }

        let schema_name = path.file_name().unwrap().to_str().unwrap();

        {
            self.add_database(schema_name);
        }

        let table_path = path.join("tables").canonicalize()?;
        let table_walker = WalkDir::new(table_path).into_iter().filter_map(|e| e.ok());

        let mut table_specifiers = vec![];

        for table_file in table_walker {
            if is_js_or_ts(&table_file) {
                let url = ModuleSpecifier::from_file_path(table_file.path()).unwrap();
                table_specifiers.push(url);
            }
        }

        Ok((schema_name.to_string(), table_specifiers))
    }

    pub fn register_tables(&mut self, schema_name: &str, loaded_tables: Vec<Table>) {
        let mut db = self.find_by_name(schema_name.to_string()).unwrap();
        for table in loaded_tables {
            db.add_table(table);
        }
    }

    pub fn find_by_name(&mut self, name: String) -> Option<&mut EngineDb> {
        self.databases.iter_mut().find(|i| i.name == name)
    }

    pub fn find_by_name_ref(&self, name: String) -> Option<&EngineDb> {
        self.databases.iter().find(|i| i.name == name)
    }

    pub fn add_database(&mut self, name: &str) {
        self.databases
            .push(EngineDb::new(self.data_path_dir.clone(), name))
    }
}

#[cfg(test)]
mod test {
    use crate::engine::SchemeJsEngine;
    use schemajs_data::shard::Shard;
    use schemajs_primitives::column::types::{DataTypes, DataValue};
    use schemajs_primitives::column::Column;
    use schemajs_primitives::table::Table;
    use schemajs_query::row::Row;
    use schemajs_query::row_json::{RowData, RowJson};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use std::thread;

    #[flaky_test::flaky_test]
    pub fn test_db_engine() {
        let db_engine = Arc::new(RwLock::new(SchemeJsEngine::new(None)));

        // Add database
        {
            let mut writer = db_engine.write().unwrap();
            writer.add_database("rust-test-random");
        } // Release the write lock

        {
            {
                let mut reader = db_engine.read().unwrap();
                let db = reader
                    .find_by_name_ref("rust-test-random".to_string())
                    .unwrap();

                assert_eq!(db.db_folder.exists(), true);
            }

            {
                let mut cols: HashMap<String, Column> = HashMap::new();
                cols.insert(
                    "id".to_string(),
                    Column {
                        name: "id".to_string(),
                        data_type: DataTypes::String,
                        default_value: None,
                        required: false,
                        comment: None,
                        primary_key: false,
                    },
                );

                let table = Table {
                    name: "users".to_string(),
                    columns: cols,
                    indexes: vec![],
                    primary_key: "".to_string(),
                    metadata: Default::default(),
                };

                let mut writer = db_engine.write().unwrap();
                let mut db = writer.find_by_name("rust-test-random".to_string()).unwrap();
                db.add_table(table);
            }
        }

        let arc = db_engine.clone();

        let ref_shard1 = Arc::clone(&arc);
        let thread_1 = thread::spawn(move || {
            let mut writer = ref_shard1.write().unwrap();
            writer
                .find_by_name_ref("rust-test-random".to_string())
                .unwrap()
                .query_manager
                .insert(RowJson {
                    value: RowData {
                        table: "users".to_string(),
                        value: json!({
                            "_uid": "97ad4bba-98c5-4a9e-80d8-6bf6302fb883",
                            "id": "1"
                        }),
                    },
                })
                .unwrap();
        });

        let ref_shard2 = Arc::clone(&arc);
        let thread_2 = thread::spawn(move || {
            let mut writer = ref_shard2.write().unwrap();
            writer
                .find_by_name_ref("rust-test-random".to_string())
                .unwrap()
                .query_manager
                .insert(RowJson {
                    value: RowData {
                        table: "users".to_string(),
                        value: json!({
                            "_uid": "2ec92148-646d-4521-974f-b4a6d422c195",
                            "id": "2"
                        }),
                    },
                })
                .unwrap();
        });

        thread_1.join().unwrap();
        thread_2.join().unwrap();

        // Assuming `temp_shards` is part of `EngineTable` and is a `RwLock<HashMap<String, Shard>>`
        {
            let mut reader = db_engine.write().unwrap();
            let mut db = reader.find_by_name("rust-test-random".to_string()).unwrap();
            let tbl = db.query_manager.tables.get("users").unwrap();
            tbl.temps.reconcile_all();

            let a = tbl.data.read().unwrap().get_element(0).unwrap();
            let b = tbl.data.read().unwrap().get_element(1).unwrap();

            let a = RowJson::from(a.as_slice());
            let b = RowJson::from(b.as_slice());

            assert_eq!(
                a.get_value(tbl.table.get_column("id").unwrap()).unwrap(),
                DataValue::String("1".to_string())
            );
            assert_eq!(
                b.get_value(tbl.table.get_column("id").unwrap()).unwrap(),
                DataValue::String("2".to_string())
            );
        }
    }
}
