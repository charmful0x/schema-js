use crate::snapshot;
use anyhow::{bail, Error, Result};
use deno_core::_ops::RustToV8;
use deno_core::url::Url;
use deno_core::{
    located_script_name, serde_v8, v8, Extension, JsRuntime, ModuleCodeString, ModuleId,
    ModuleSpecifier, RuntimeOptions,
};
use schemajs_config::SchemeJsConfig;
use schemajs_engine::engine::SchemeJsEngine;
use schemajs_module_loader::ts_module_loader::TypescriptModuleLoader;
use schemajs_primitives::database::Database;
use schemajs_primitives::table::Table;
use schemajs_workers::context::{MainWorkerRuntimeOpts, WorkerRuntimeOpts};
use serde::{Deserialize, Serialize};
use std::cell::{RefCell, RefMut};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use walkdir::{DirEntry, WalkDir};

pub struct SchemeJsRuntime {
    pub js_runtime: JsRuntime,
    pub config: WorkerRuntimeOpts,
    pub config_file: PathBuf,
    pub data_path_folder: Option<PathBuf>,
    pub current_folder: PathBuf,
    pub engine: Arc<SchemeJsEngine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerContextInitOpts {
    pub config_path: PathBuf,
    pub data_path: Option<PathBuf>,
}

impl SchemeJsRuntime {
    pub async fn new(opts: WorkerContextInitOpts) -> Result<Self> {
        let WorkerContextInitOpts {
            config_path,
            data_path,
        } = opts;

        // Determine the base path by joining the current directory with the config path
        let base_path = std::env::current_dir()?.join(&config_path);

        // Determine the appropriate folder path and config file path
        let (folder_path, config_file) = if base_path.is_dir() {
            (base_path.clone(), base_path.join("SchemeJS.toml"))
        } else {
            let folder_path = base_path.parent().map_or_else(
                || std::env::current_dir(),
                |parent| Ok(parent.to_path_buf()),
            )?;
            (folder_path.clone(), base_path)
        };

        let config = SchemeJsConfig::new(config_file.clone())?;

        let extensions: Vec<Extension> = vec![
            schemajs_primitives::sjs_primitives::init_ops(),
            schemajs_core::sjs_core::init_ops(),
            schemajs_engine::sjs_engine::init_ops(),
        ];

        let runtime_opts = RuntimeOptions {
            extensions,
            is_main: true,
            shared_array_buffer_store: None,
            compiled_wasm_module_store: None,
            startup_snapshot: snapshot::snapshot(),
            module_loader: Some(Rc::new(TypescriptModuleLoader::default())),
            ..Default::default()
        };

        let mut js_runtime = JsRuntime::new(runtime_opts);

        // Bootstrapping Stage
        {
            let script = format!("globalThis.bootstrap()");
            js_runtime
                .execute_script(located_script_name!(), ModuleCodeString::from(script))
                .expect("Failed to execute bootstrap script");
        }

        let config_opts = WorkerRuntimeOpts::Main(MainWorkerRuntimeOpts { config });
        let mut engine = SchemeJsEngine::new(data_path.clone());
        Self::load(&config_opts, &mut js_runtime, &folder_path, &mut engine)
            .await
            .unwrap();

        let engine = Arc::new(engine);
        {
            // Put reference to engine
            let op_state_rc = js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();
            op_state.put::<Arc<SchemeJsEngine>>(engine.clone());
        }

        Ok(Self {
            js_runtime,
            config: config_opts,
            config_file,
            current_folder: folder_path,
            engine,
            data_path_folder: data_path.clone(),
        })
    }

    pub async fn load(
        config: &WorkerRuntimeOpts,
        js_runtime: &mut JsRuntime,
        current_folder: &PathBuf,
        engine: &mut SchemeJsEngine,
    ) -> Result<()> {
        match &config {
            WorkerRuntimeOpts::Main(conf) => {
                let databases = conf.config.workspace.databases.clone();

                for database_path in databases {
                    let path = current_folder.join(&database_path);
                    let (scheme_name, table_specifiers) = engine.load_database_schema(&path)?;
                    let mut tables = vec![];
                    for table_specifier in table_specifiers {
                        let (_, _, tbl) =
                            Self::load_table(js_runtime, table_specifier).await.unwrap();
                        tables.push(tbl);
                    }

                    engine.register_tables(scheme_name.as_str(), tables);
                }

                Ok(())
            }
        }
    }

    async fn load_table(
        js_runtime: &mut JsRuntime,
        specifier: ModuleSpecifier,
    ) -> Result<(ModuleSpecifier, ModuleId, Table)> {
        let mod_id = js_runtime.load_side_es_module(&specifier).await?;
        let _ = js_runtime.mod_evaluate(mod_id).await?;

        let mut table = {
            let mod_scope = js_runtime.get_module_namespace(mod_id)?;
            let scope = &mut js_runtime.handle_scope();
            {
                let mod_obj = mod_scope.open(scope).to_object(scope).unwrap();
                let default_function_key = v8::String::new(scope, "default").unwrap();
                let func_obj = mod_obj.get(scope, default_function_key.into()).unwrap();
                let func = v8::Local::<v8::Function>::try_from(func_obj)?;
                let undefined = v8::undefined(scope);

                /// TODO: Handle this error
                let mut exc = func.call(scope, undefined.into(), &[]).unwrap(); /*
                                                                                .ok_or_else(Error::msg("Table could not be read"))?*/

                let is_promise = exc.is_promise();

                if is_promise {
                    let promise = v8::Local::<v8::Promise>::try_from(exc).unwrap();
                    match promise.state() {
                        v8::PromiseState::Pending => {}
                        v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => {
                            exc = promise.result(scope);
                        }
                    }
                }

                deno_core::serde_v8::from_v8::<Table>(scope, exc)?
            }
        };

        table.init();

        table.metadata.set_module_id(mod_id);

        Ok((specifier, mod_id, table))
    }
}

#[cfg(test)]
mod test {
    use crate::manager::task::{Task, TaskCallback};
    use crate::manager::task_duration::TaskDuration;
    use crate::manager::SchemeJsManager;
    use crate::runtime::{SchemeJsRuntime, WorkerContextInitOpts};
    use deno_core::{located_script_name, serde_json, v8};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    pub async fn test_runtime_config_as_folder() -> anyhow::Result<()> {
        let create_rt = SchemeJsRuntime::new(WorkerContextInitOpts {
            config_path: PathBuf::from("./test_cases/default-db"),
            data_path: None,
        })
        .await?;

        assert_eq!(
            create_rt.current_folder,
            std::env::current_dir()
                .unwrap()
                .join("./test_cases/default-db")
        );
        assert_eq!(
            create_rt.config_file,
            std::env::current_dir()
                .unwrap()
                .join("./test_cases/default-db/SchemeJS.toml")
        );

        Ok(())
    }

    #[tokio::test]
    pub async fn test_runtime_insert() -> anyhow::Result<()> {
        let data_path = std::env::current_dir()
            .unwrap()
            .join(PathBuf::from("./test_cases/data"));
        let now = std::time::Instant::now();
        {
            let mut create_rt = SchemeJsRuntime::new(WorkerContextInitOpts {
                config_path: PathBuf::from("./test_cases/default-db"),
                data_path: Some(data_path.clone()),
            })
            .await?;

            let num_inserts = 10_000;
            let mut script = String::new();

            for i in 0..num_inserts {
                script.push_str(&format!(
                    r#"globalThis.SchemeJS.insert("{}", "{}", {});"#,
                    "public",
                    "users",
                    serde_json::json!({
                        "id": "ABCD"
                    })
                    .to_string()
                ));
            }
            create_rt
                .js_runtime
                .execute_script(located_script_name!(), script)?;
        }
        let elapsed = now.elapsed();
        println!("Elapsed: {:.5?}", elapsed);

        std::fs::remove_dir_all(data_path).unwrap();

        Ok(())
    }

    #[tokio::test]
    pub async fn test_runtime_insert_with_manager() -> anyhow::Result<()> {
        let data_path = std::env::current_dir()
            .unwrap()
            .join(PathBuf::from("./test_cases/data"));
        let now = std::time::Instant::now();

        {
            let mut rt = SchemeJsRuntime::new(WorkerContextInitOpts {
                config_path: PathBuf::from("./test_cases/default-db"),
                data_path: Some(data_path.clone()),
            })
            .await?;

            let mut manager = SchemeJsManager::new(rt.engine.clone());

            manager.add_task(Task::new(
                "1".to_string(),
                Box::new(move |rt| {
                    for x in rt.databases.iter() {
                        let query_manager = &x.query_manager;
                        for table in query_manager.table_names.read().unwrap().iter() {
                            let table = query_manager.tables.get(table).unwrap();
                            table.temps.reconcile_all();
                        }
                    }
                    Ok(())
                }),
                TaskDuration::Defined(Duration::from_millis(250)),
            ));

            // manager.start_tasks();

            let num_inserts = 9500;
            let mut script = String::new();
            println!("To be inserted");

            for i in 0..num_inserts {
                script.push_str(&format!(
                    r#"globalThis.SchemeJS.insert("{}", "{}", {});"#,
                    "public",
                    "users",
                    serde_json::json!({
                        "id": "ABCD"
                    })
                    .to_string()
                ));
            }

            println!("Inserted");

            rt.js_runtime
                .execute_script(located_script_name!(), script)?;

            // Example: Stop the reconciler and other tasks after some time
            //tokio::time::sleep(Duration::from_secs(20)).await;
            //manager.stop_tasks();

            println!("Executed");
        }

        Ok(())
    }

    #[tokio::test]
    pub async fn test_runtime_config_as_file() -> anyhow::Result<()> {
        let create_rt = SchemeJsRuntime::new(WorkerContextInitOpts {
            config_path: PathBuf::from("./test_cases/default-db/CustomSchemeJS.toml"),
            data_path: None,
        })
        .await?;

        assert_eq!(
            create_rt.current_folder,
            std::env::current_dir()
                .unwrap()
                .join("./test_cases/default-db")
        );
        assert_eq!(
            create_rt.config_file,
            std::env::current_dir()
                .unwrap()
                .join("./test_cases/default-db/CustomSchemeJS.toml")
        );

        Ok(())
    }
}
