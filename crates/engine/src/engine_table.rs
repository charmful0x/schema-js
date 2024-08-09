use crate::query_error::{InsertionError, QueryError};
use crate::serializer::borsh::BorshRowSerializer;
use crate::serializer::RowSerializer;
use crate::validation_error::ValidationError;
use deno_core::serde_json;
use schemajs_data::map_shard::MapShard;
use schemajs_data::temp_map_shard::TempMapShard;
use schemajs_data::temp_offset_types::TempOffsetTypes;
use schemajs_dirs::create_schema_js_table;
use schemajs_primitives::table::Table;
use schemajs_primitives::types::DataTypes;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Debug)]
pub struct EngineTable {
    pub tbl_folder: PathBuf,
    pub prim_table: Table,
    pub data: Arc<RwLock<MapShard>>,
    pub temp_shards: TempMapShard,
    pub serializer: Arc<dyn RowSerializer>,
}

impl EngineTable {
    pub fn new(base_path: Option<PathBuf>, db: &str, table: Table) -> Self {
        let table_folder_path = create_schema_js_table(base_path, db, table.name.as_str());

        let data = Arc::new(RwLock::new(MapShard::new(
            table_folder_path.clone(),
            "data_",
            None,
        )));

        EngineTable {
            tbl_folder: table_folder_path.clone(),
            prim_table: table,
            data: data.clone(),
            temp_shards: TempMapShard::new(
                table_folder_path,
                data.clone(),
                TempOffsetTypes::Custom(Some(1000)),
                "datatemp-",
            ),
            serializer: Arc::new(BorshRowSerializer::default()),
        }
    }

    fn validate_row_value(&self, item: &serde_json::Value) -> Result<(), ValidationError> {
        for (name, column) in self.prim_table.columns.iter() {
            let value = item.get(name);

            if value.is_none() {
                if column.required {
                    return Err(ValidationError::MissingColumn(name.clone()));
                } else {
                    return Ok(());
                }
            }

            let value = value.unwrap();

            match column.data_type {
                DataTypes::String => {
                    if !value.is_string() {
                        return Err(ValidationError::ExpectedString(name.clone()));
                    }
                }
                DataTypes::Boolean => {
                    if !value.is_boolean() {
                        return Err(ValidationError::ExpectedBoolean(name.clone()));
                    }
                }
            }
        }

        Ok(())
    }

    pub fn insert_row(&self, item: serde_json::Value) -> Result<(), QueryError> {
        let validate = self.validate_row_value(&item);
        validate.map_err(InsertionError::ValidationError)?;
        let val = self
            .serializer
            .serialize(&item)
            .map_err(InsertionError::SerializationError)?;
        self.temp_shards.insert_row(val);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::engine_table::EngineTable;
    use schemajs_primitives::column::Column;
    use schemajs_primitives::table::Table;
    use schemajs_primitives::types::DataTypes;
    use std::collections::HashMap;

    fn get_common_table() -> Table {
        let mut cols: HashMap<String, Column> = HashMap::new();
        cols.insert(
            "id".to_string(),
            Column {
                name: "id".to_string(),
                data_type: DataTypes::String,
                default_value: None,
                required: false,
                comment: None,
            },
        );

        cols.insert(
            "enabled".to_string(),
            Column {
                name: "enabled".to_string(),
                data_type: DataTypes::Boolean,
                default_value: None,
                required: true,
                comment: None,
            },
        );

        let table = Table {
            name: "users".to_string(),
            columns: cols,
            module_id: None,
        };

        table
    }

    #[tokio::test]
    pub async fn test_row_correct_validation() {
        let table = get_common_table();
        let engine_table = EngineTable::new(None, "public", table);
        engine_table
            .validate_row_value(&serde_json::json!({
                "id": "Hello",
                "enabled": true
            }))
            .unwrap();
    }

    #[tokio::test]
    pub async fn test_row_invalid_boolean() {
        let table = get_common_table();
        let engine_table = EngineTable::new(None, "public", table);
        let validate = engine_table.validate_row_value(&serde_json::json!({
            "id": "1",
            "enabled": ""
        }));

        assert!(validate.is_err());
        assert!(validate.err().unwrap().is_expected_boolean());

        let validate = engine_table.validate_row_value(&serde_json::json!({
            "id": "1"
        }));
        assert!(validate.err().unwrap().is_missing_column());
    }
}
