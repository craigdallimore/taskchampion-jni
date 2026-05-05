use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobjectArray};
use jni::JNIEnv;
use taskchampion::{Replica, StorageConfig, Operations, Operation, Status, Tag, Annotation, ServerConfig};
use taskchampion::server::AwsCredentials;
use uuid::Uuid;
use chrono::Utc;
use log::{info, error, warn};
use serde_json;
use std::env;
use std::panic;
use std::sync::{Arc, Mutex};
use dashmap::DashMap;
use lazy_static::lazy_static;
use crate::logging::init_android_logger;

/// Configure TLS to use bundled certificates instead of native Android certificate store
/// This prevents SIGABRT crashes when rustls-native-certs fails to find Android certificates
fn configure_android_tls() {
    // Disable native certificate loading and force webpki-roots usage
    env::set_var("RUSTLS_NATIVE_CERTS", "0");
    // Force AWS SDK to use bundled certificates
    env::set_var("AWS_USE_BUNDLED_CA", "1");
    info!("Configured TLS to use bundled certificates for Android compatibility");
}

// Per-replica mutex registry for thread safety
lazy_static! {
    static ref REPLICA_LOCKS: DashMap<jlong, Arc<Mutex<()>>> = DashMap::new();
}

// Fully-qualified names of the Java exception classes thrown by this binding.
const EXC_BASE: &str = "com/tasksquire/data/storage/TaskChampionException";
const EXC_INVALID_REPLICA: &str = "com/tasksquire/data/storage/InvalidReplicaException";
const EXC_INVALID_UUID: &str = "com/tasksquire/data/storage/InvalidUuidException";
const EXC_INVALID_STATUS: &str = "com/tasksquire/data/storage/InvalidStatusException";
const EXC_INVALID_TAG: &str = "com/tasksquire/data/storage/InvalidTagException";
const EXC_REPLICA_INIT: &str = "com/tasksquire/data/storage/ReplicaInitializationException";
const EXC_SYNC: &str = "com/tasksquire/data/storage/SyncException";
const EXC_STORAGE: &str = "com/tasksquire/data/storage/TaskChampionStorageException";

/// Best-effort extraction of a human-readable string from a panic payload.
fn panic_msg(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<panic with non-string payload>".to_string()
    }
}

/// Wrap a JNI entry-point body in `panic::catch_unwind` so a Rust panic
/// never crosses the FFI boundary into the JVM. On panic, throws
/// TaskChampionException with the panic message and returns the supplied
/// default sentinel.
macro_rules! catch_panics {
    ($env:expr, $method:expr, $default:expr, $body:block) => {{
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { $body }));
        match panic_result {
            Ok(value) => value,
            Err(payload) => {
                let msg = panic_msg(&payload);
                error!("Panic in {}: {}", $method, msg);
                throw($env, EXC_BASE, &format!("Internal panic in {}: {}", $method, msg));
                $default
            }
        }
    }};
}

/// Throw a Java exception of the given class with the given message.
/// If a JVM exception is already pending, this is a no-op so that the
/// original exception is preserved. Failures to throw are logged.
fn throw<'local>(env: &mut JNIEnv<'local>, class: &str, msg: &str) {
    if env.exception_check().unwrap_or(false) {
        return;
    }
    if let Err(e) = env.throw_new(class, msg) {
        error!("Failed to throw {}: {:?}", class, e);
    }
}

/// Read a JString parameter into a Rust String. Throws
/// TaskChampionStorageException on JNI marshalling failure (rare).
fn read_jstring<'local, 's>(
    env: &mut JNIEnv<'local>,
    jstr: &JString<'s>,
    param_name: &str,
) -> Option<String> {
    match env.get_string(jstr) {
        Ok(s) => Some(s.into()),
        Err(e) => {
            error!("Failed to read JNI string parameter '{}': {:?}", param_name, e);
            throw(
                env,
                EXC_STORAGE,
                &format!("Failed to read parameter '{}' from JVM: {}", param_name, e),
            );
            None
        }
    }
}

/// Parse a string as a v4 UUID. Throws InvalidUuidException on failure.
fn parse_uuid(env: &mut JNIEnv, uuid_str: &str) -> Option<Uuid> {
    match Uuid::parse_str(uuid_str) {
        Ok(u) => Some(u),
        Err(e) => {
            throw(
                env,
                EXC_INVALID_UUID,
                &format!("Invalid UUID '{}': {}", uuid_str, e),
            );
            None
        }
    }
}

/// Acquire the per-replica mutex and run a closure with exclusive access
/// to the replica. The lock is released before this function returns.
///
/// Behaviour:
/// - If `replica_ptr` is null or no longer registered, throws
///   InvalidReplicaException and returns `default` (the JVM observes
///   the exception and ignores the return value).
/// - If the closure returns `Err(msg)`, throws TaskChampionStorageException
///   with that message and returns `default`.
/// - If the closure returns `Ok(value)`, returns `value`.
///
/// All JNI marshalling of inputs and outputs should happen outside this
/// function so the lock is held only for the replica work itself.
fn run_with_replica<'local, F, R>(
    env: &mut JNIEnv<'local>,
    replica_ptr: jlong,
    method_name: &str,
    default: R,
    f: F,
) -> R
where
    F: FnOnce(&mut Replica) -> Result<R, String>,
{
    if replica_ptr == 0 {
        throw(
            env,
            EXC_INVALID_REPLICA,
            &format!("Null replica pointer in {}", method_name),
        );
        return default;
    }

    let lock_arc = match REPLICA_LOCKS.get(&replica_ptr) {
        Some(entry) => entry.clone(),
        None => {
            throw(
                env,
                EXC_INVALID_REPLICA,
                &format!(
                    "Invalid replica pointer in {} (not registered or already destroyed)",
                    method_name
                ),
            );
            return default;
        }
    };

    let result = {
        let _guard = match lock_arc.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("Replica mutex poisoned in {}, recovering", method_name);
                poisoned.into_inner()
            }
        };
        let replica = unsafe { &mut *(replica_ptr as *mut Replica) };
        f(replica)
    };

    match result {
        Ok(value) => value,
        Err(msg) => {
            error!("{}", msg);
            throw(env, EXC_STORAGE, &msg);
            default
        }
    }
}



// Helper function to create empty string array for error cases
fn create_empty_string_array<'local>(env: &mut JNIEnv<'local>) -> jobjectArray {
    match env.find_class("java/lang/String") {
        Ok(string_class) => {
            match env.new_object_array(0, &string_class, JObject::null()) {
                Ok(empty_array) => empty_array.into_raw(),
                Err(e) => {
                    error!("Failed to create empty array: {:?}", e);
                    std::ptr::null_mut()
                }
            }
        }
        Err(e) => {
            error!("Failed to find String class: {:?}", e);
            std::ptr::null_mut()
        }
    }
}

// Helper function to create string array
fn create_string_array<'local>(env: &mut JNIEnv<'local>, strings: Vec<String>) -> jobjectArray {
    match env.find_class("java/lang/String") {
        Ok(string_class) => {
            match env.new_object_array(strings.len() as i32, &string_class, JObject::null()) {
                Ok(java_array) => {
                    for (i, s) in strings.iter().enumerate() {
                        match env.new_string(s) {
                            Ok(java_string) => {
                                if let Err(e) = env.set_object_array_element(&java_array, i as i32, java_string) {
                                    error!("Failed to set array element {}: {:?}", i, e);
                                    return create_empty_string_array(env);
                                }
                            }
                            Err(e) => {
                                error!("Failed to create Java string: {:?}", e);
                                return create_empty_string_array(env);
                            }
                        }
                    }
                    java_array.into_raw()
                }
                Err(e) => {
                    error!("Failed to create Java array: {:?}", e);
                    create_empty_string_array(env)
                }
            }
        }
        Err(e) => {
            error!("Failed to find String class: {:?}", e);
            create_empty_string_array(env)
        }
    }
}

// Lifecycle management

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeInitialize<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    data_dir: JString<'local>,
) -> jlong {
    init_android_logger();
    configure_android_tls();

    catch_panics!(&mut env, "nativeInitialize", 0, {
        let data_dir_str = match read_jstring(&mut env, &data_dir, "data_dir") {
            Some(s) => s,
            None => return 0,
        };

        info!("Initializing Replica with data directory: {}", data_dir_str);

        let storage_config = StorageConfig::OnDisk {
            taskdb_dir: data_dir_str.into(),
            create_if_missing: true,
            access_mode: taskchampion::storage::AccessMode::ReadWrite,
        };

        let storage = match storage_config.into_storage() {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create storage: {:?}", e);
                throw(&mut env, EXC_REPLICA_INIT, &format!("Failed to create storage: {}", e));
                return 0;
            }
        };

        let replica = Replica::new(storage);
        let boxed_replica = Box::new(replica);
        let replica_ptr = Box::into_raw(boxed_replica) as jlong;
        REPLICA_LOCKS.insert(replica_ptr, Arc::new(Mutex::new(())));

        info!("Replica initialized successfully, pointer: {}", replica_ptr);
        replica_ptr
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeDestroy(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) {
    catch_panics!(&mut env, "nativeDestroy", (), {
        if replica_ptr == 0 {
            throw(&mut env, EXC_INVALID_REPLICA, "Cannot destroy a null replica pointer");
            return;
        }

        info!("Destroying Replica with pointer: {}", replica_ptr);

        if REPLICA_LOCKS.remove(&replica_ptr).is_none() {
            throw(
                &mut env,
                EXC_INVALID_REPLICA,
                &format!("Replica pointer {} is not registered (already destroyed?)", replica_ptr),
            );
            return;
        }

        unsafe {
            let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
            drop(boxed_replica);
            info!("Replica destroyed successfully");
        }
    })
}

// Transaction control

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeUndo(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) -> jboolean {
    catch_panics!(&mut env, "nativeUndo", 0, {
        enum UndoOutcome {
            NoOpsToUndo,
            Reversed,
            ConcurrentChanges,
        }

        let outcome = run_with_replica(&mut env, replica_ptr, "nativeUndo", UndoOutcome::NoOpsToUndo, |replica| {
            let undo_ops = replica
                .get_undo_operations()
                .map_err(|e| format!("Failed to get undo operations: {}", e))?;
            if undo_ops.is_empty() {
                return Ok(UndoOutcome::NoOpsToUndo);
            }
            match replica.commit_reversed_operations(undo_ops) {
                Ok(true) => Ok(UndoOutcome::Reversed),
                Ok(false) => Ok(UndoOutcome::ConcurrentChanges),
                Err(e) => Err(format!("Failed to commit undo operations: {}", e)),
            }
        });

        match outcome {
            UndoOutcome::Reversed => {
                info!("Undo operation completed successfully");
                1
            }
            UndoOutcome::NoOpsToUndo => {
                info!("No operations to undo");
                0
            }
            UndoOutcome::ConcurrentChanges => {
                warn!("Undo operation failed - concurrent changes detected");
                0
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeAddUndoPoint(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) {
    catch_panics!(&mut env, "nativeAddUndoPoint", (), {
        run_with_replica(&mut env, replica_ptr, "nativeAddUndoPoint", (), |replica| {
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to add undo point: {}", e))?;
            info!("Undo point added");
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeRebuildWorkingSet(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    renumber: jboolean,
) {
    catch_panics!(&mut env, "nativeRebuildWorkingSet", (), {
        let renumber = renumber != 0;
        run_with_replica(&mut env, replica_ptr, "nativeRebuildWorkingSet", (), |replica| {
            replica
                .rebuild_working_set(renumber)
                .map_err(|e| format!("Failed to rebuild working set: {}", e))?;
            info!("Working set rebuilt (renumber={})", renumber);
            Ok(())
        });
    })
}

// Task creation and basic operations

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeCreateTask(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
) {
    catch_panics!(&mut env, "nativeCreateTask", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };

        run_with_replica(&mut env, replica_ptr, "nativeCreateTask", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .create_task(task_uuid, &mut ops)
                .map_err(|e| format!("Failed to create task: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("entry", Some(now.clone()), &mut ops)
                .map_err(|e| format!("Failed to set entry timestamp: {}", e))?;
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit create task operations: {}", e))?;
            info!("Task created successfully: {}", uuid_str);
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskSetDescription(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    desc: JString,
) {
    catch_panics!(&mut env, "nativeTaskSetDescription", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let description = match read_jstring(&mut env, &desc, "description") { Some(s) => s, None => return };

        run_with_replica(&mut env, replica_ptr, "nativeTaskSetDescription", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.set_description(description, &mut ops)
                .map_err(|e| format!("Failed to set task description: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit set description operations: {}", e))?;
            info!("Task description updated successfully: {}", uuid_str);
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskSetStatus(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    status: JString,
) {
    catch_panics!(&mut env, "nativeTaskSetStatus", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let status_str = match read_jstring(&mut env, &status, "status") { Some(s) => s, None => return };

        let task_status = match status_str.as_str() {
            "pending" => Status::Pending,
            "completed" => Status::Completed,
            "deleted" => Status::Deleted,
            _ => {
                throw(
                    &mut env,
                    EXC_INVALID_STATUS,
                    &format!("Invalid status '{}'; expected one of: pending, completed, deleted", status_str),
                );
                return;
            }
        };

        run_with_replica(&mut env, replica_ptr, "nativeTaskSetStatus", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.set_status(task_status, &mut ops)
                .map_err(|e| format!("Failed to set task status: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit set status operations: {}", e))?;
            info!("Task status updated successfully: {} -> {}", uuid_str, status_str);
            Ok(())
        });
    })
}

// Task property management

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskSetValue(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    key: JString,
    value: JString,
) {
    catch_panics!(&mut env, "nativeTaskSetValue", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let key_str = match read_jstring(&mut env, &key, "key") { Some(s) => s, None => return };

        let value_opt: Option<String> = if value.is_null() {
            None
        } else {
            match read_jstring(&mut env, &value, "value") { Some(s) => Some(s), None => return }
        };

        let value_present = value_opt.is_some();
        let key_for_log = key_str.clone();

        run_with_replica(&mut env, replica_ptr, "nativeTaskSetValue", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.set_value(&key_str, value_opt, &mut ops)
                .map_err(|e| format!("Failed to set task value: {}", e))?;
            if key_str != "modified" {
                let now = Utc::now().timestamp().to_string();
                task.set_value("modified", Some(now), &mut ops)
                    .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            }
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit set value operations: {}", e))?;
            info!(
                "Task value updated successfully: {} -> {}={}",
                uuid_str,
                key_for_log,
                if value_present { "Some(_)" } else { "None" }
            );
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskAddTag(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    tag: JString,
) {
    catch_panics!(&mut env, "nativeTaskAddTag", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let tag_str = match read_jstring(&mut env, &tag, "tag") { Some(s) => s, None => return };

        let task_tag = match Tag::try_from(tag_str.as_str()) {
            Ok(t) => t,
            Err(e) => {
                throw(&mut env, EXC_INVALID_TAG, &format!("Invalid tag '{}': {}", tag_str, e));
                return;
            }
        };

        run_with_replica(&mut env, replica_ptr, "nativeTaskAddTag", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.add_tag(&task_tag, &mut ops)
                .map_err(|e| format!("Failed to add tag to task: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit add tag operations: {}", e))?;
            info!("Tag added successfully: {} -> {}", uuid_str, tag_str);
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskRemoveTag(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    tag: JString,
) {
    catch_panics!(&mut env, "nativeTaskRemoveTag", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let tag_str = match read_jstring(&mut env, &tag, "tag") { Some(s) => s, None => return };

        let task_tag = match Tag::try_from(tag_str.as_str()) {
            Ok(t) => t,
            Err(e) => {
                throw(&mut env, EXC_INVALID_TAG, &format!("Invalid tag '{}': {}", tag_str, e));
                return;
            }
        };

        run_with_replica(&mut env, replica_ptr, "nativeTaskRemoveTag", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.remove_tag(&task_tag, &mut ops)
                .map_err(|e| format!("Failed to remove tag from task: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit remove tag operations: {}", e))?;
            info!("Tag removed successfully: {} -> {}", uuid_str, tag_str);
            Ok(())
        });
    })
}

// Annotations

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskAddAnnotation(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    desc: JString,
) {
    catch_panics!(&mut env, "nativeTaskAddAnnotation", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };
        let description = match read_jstring(&mut env, &desc, "description") { Some(s) => s, None => return };

        run_with_replica(&mut env, replica_ptr, "nativeTaskAddAnnotation", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            let annotation = Annotation {
                entry: Utc::now(),
                description,
            };
            task.add_annotation(annotation, &mut ops)
                .map_err(|e| format!("Failed to add annotation to task: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit add annotation operations: {}", e))?;
            info!("Annotation added successfully to task: {}", uuid_str);
            Ok(())
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeTaskRemoveAnnotation(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
    entry_timestamp: jlong,
) {
    catch_panics!(&mut env, "nativeTaskRemoveAnnotation", (), {
        let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return };
        let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return };

        let entry_time = match chrono::DateTime::from_timestamp(entry_timestamp, 0) {
            Some(dt) => dt,
            None => {
                throw(
                    &mut env,
                    "java/lang/IllegalArgumentException",
                    &format!("Invalid annotation entry timestamp: {}", entry_timestamp),
                );
                return;
            }
        };

        run_with_replica(&mut env, replica_ptr, "nativeTaskRemoveAnnotation", (), |replica| {
            let mut ops = Operations::new();
            let mut task = replica
                .get_task(task_uuid)
                .map_err(|e| format!("Failed to get task: {}", e))?
                .ok_or_else(|| format!("Task not found: {}", uuid_str))?;
            task.remove_annotation(entry_time, &mut ops)
                .map_err(|e| format!("Failed to remove annotation from task: {}", e))?;
            let now = Utc::now().timestamp().to_string();
            task.set_value("modified", Some(now), &mut ops)
                .map_err(|e| format!("Failed to set modified timestamp: {}", e))?;
            drop(task);
            replica
                .commit_operations(ops)
                .map_err(|e| format!("Failed to commit remove annotation operations: {}", e))?;
            info!("Annotation removed successfully from task: {} at timestamp {}", uuid_str, entry_timestamp);
            Ok(())
        });
    })
}

// Data retrieval

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetAllTaskUuids<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
) -> jobjectArray {
    catch_panics!(&mut env, "nativeGetAllTaskUuids", std::ptr::null_mut(), {
        let task_uuids = run_with_replica(&mut env, replica_ptr, "nativeGetAllTaskUuids", Vec::<String>::new(), |replica| {
            let tasks = replica
                .all_tasks()
                .map_err(|e| format!("Failed to get all tasks: {}", e))?;
            info!("Found {} task UUIDs", tasks.len());
            Ok(tasks.keys().map(|uuid| uuid.to_string()).collect())
        });

        create_string_array(&mut env, task_uuids)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetTaskData<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
) -> JString<'local> {
    catch_panics!(&mut env, "nativeGetTaskData", JObject::null().into(), {
    let uuid_str = match read_jstring(&mut env, &uuid, "uuid") { Some(s) => s, None => return JObject::null().into() };
    let task_uuid = match parse_uuid(&mut env, &uuid_str) { Some(u) => u, None => return JObject::null().into() };

    // None signals "task not found" — returned to Java as null. Storage errors throw.
    let json_result: Option<String> = run_with_replica(&mut env, replica_ptr, "nativeGetTaskData", None, |replica| {
        let task = match replica
            .get_task(task_uuid)
            .map_err(|e| format!("Failed to get task: {}", e))?
        {
            Some(t) => t,
            None => {
                info!("Task not found: {}", uuid_str);
                return Ok(None);
            }
        };

        let mut task_map = std::collections::HashMap::new();

        if let Some(task_data) = replica
            .get_task_data(task_uuid)
            .map_err(|e| format!("Failed to get task data: {}", e))?
        {
            for (key, value) in task_data.iter() {
                task_map.insert(key.clone(), value.clone());
            }
        }

        let tags: Vec<Tag> = task.get_tags().collect();
        for (i, tag) in tags.iter().enumerate() {
            task_map.insert(format!("tag_{}", i), tag.to_string());
        }

        let annotations: Vec<Annotation> = task.get_annotations().collect();
        for (i, annotation) in annotations.iter().enumerate() {
            task_map.insert(format!("annotation_{}_entry", i), annotation.entry.timestamp().to_string());
            task_map.insert(format!("annotation_{}_description", i), annotation.description.clone());
        }

        task_map.insert("uuid".to_string(), uuid_str.clone());

        let json = serde_json::to_string(&task_map)
            .map_err(|e| format!("Failed to serialize task data to JSON: {}", e))?;
        info!("Retrieved task data for: {}", uuid_str);
        Ok(Some(json))
    });

    match json_result {
        Some(json) => match env.new_string(&json) {
            Ok(java_string) => java_string,
            Err(e) => {
                error!("Failed to create Java string for task data: {:?}", e);
                throw(&mut env, EXC_STORAGE, &format!("Failed to marshal task data: {}", e));
                JObject::null().into()
            }
        },
        None => JObject::null().into(),
    }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetUuidForIndex<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
    index: jint,
) -> JString<'local> {
    catch_panics!(&mut env, "nativeGetUuidForIndex", JObject::null().into(), {
    let uuid_string: Option<String> = run_with_replica(&mut env, replica_ptr, "nativeGetUuidForIndex", None, |replica| {
        let working_set = replica
            .working_set()
            .map_err(|e| format!("Failed to get working set: {}", e))?;
        // TaskWarrior IDs are 1-based, so subtract 1 for 0-based index.
        let result = (index > 0 && (index as usize) <= working_set.len())
            .then(|| working_set.by_index((index as usize) - 1))
            .flatten()
            .filter(|uuid| !uuid.is_nil())
            .map(|uuid| uuid.to_string());
        match result.as_ref() {
            Some(s) => info!("Found UUID {} for index {}", s, index),
            None => info!("No task found at index {}", index),
        }
        Ok(result)
    });

    match uuid_string {
        Some(s) => match env.new_string(s) {
            Ok(jstr) => jstr,
            Err(e) => {
                error!("Failed to create JString for UUID: {:?}", e);
                JObject::null().into()
            }
        },
        None => JObject::null().into(),
    }
    })
}

// Synchronization

/// Validate and convert a non-empty encryption secret string.
fn parse_encryption_secret(env: &mut JNIEnv, secret: &str) -> Option<Vec<u8>> {
    if secret.is_empty() {
        throw(env, EXC_SYNC, "encryptionSecret must not be empty");
        None
    } else {
        Some(secret.as_bytes().to_vec())
    }
}

/// Run a sync against the supplied ServerConfig, translating any failure
/// into a SyncException. Caller is responsible for translating its inputs
/// into a ServerConfig and invoking this helper.
fn do_sync(env: &mut JNIEnv, replica_ptr: jlong, method_name: &str, server_config: ServerConfig) {
    info!("Starting sync via {}", method_name);
    configure_android_tls();

    enum SyncFailure {
        ServerCreate(String),
        Failed(String),
        TlsPanic,
    }

    let result: Result<(), SyncFailure> = run_with_replica(
        env,
        replica_ptr,
        method_name,
        Err(SyncFailure::Failed("lock unavailable".to_string())),
        |replica| {
            let mut server = match server_config.into_server() {
                Ok(s) => s,
                Err(e) => return Ok(Err(SyncFailure::ServerCreate(format!("{}", e)))),
            };

            let sync_result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                replica.sync(&mut server, false)
            }));

            match sync_result {
                Ok(Ok(())) => {
                    info!("Sync completed successfully");
                    if let Err(e) = replica.rebuild_working_set(true) {
                        error!("Failed to rebuild working set after sync: {:?}", e);
                    } else {
                        info!("Working set rebuilt after sync");
                    }
                    Ok(Ok(()))
                }
                Ok(Err(e)) => Ok(Err(SyncFailure::Failed(format!("{}", e)))),
                Err(panic_err) => {
                    error!("Sync operation panicked (likely TLS certificate issue): {:?}", panic_err);
                    Ok(Err(SyncFailure::TlsPanic))
                }
            }
        },
    );

    match result {
        Ok(()) => {}
        Err(SyncFailure::ServerCreate(msg)) => {
            throw(env, EXC_SYNC, &format!("Failed to create server: {}", msg));
        }
        Err(SyncFailure::Failed(msg)) => {
            throw(env, EXC_SYNC, &format!("Sync failed: {}", msg));
        }
        Err(SyncFailure::TlsPanic) => {
            throw(
                env,
                EXC_SYNC,
                "Sync failed due to a TLS-related panic in the underlying library (a known limitation with AWS sync on Android).",
            );
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeSyncGcp(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    bucket: JString,
    credential_path: JString,
    encryption_secret: JString,
) {
    catch_panics!(&mut env, "nativeSyncGcp", (), {
        let bucket = match read_jstring(&mut env, &bucket, "bucket") { Some(s) => s, None => return };
        let credential_path = if credential_path.is_null() {
            None
        } else {
            match read_jstring(&mut env, &credential_path, "credentialPath") {
                Some(s) => Some(s),
                None => return,
            }
        };
        let encryption_secret_str = match read_jstring(&mut env, &encryption_secret, "encryptionSecret") {
            Some(s) => s,
            None => return,
        };
        let encryption_secret = match parse_encryption_secret(&mut env, &encryption_secret_str) {
            Some(b) => b,
            None => return,
        };

        let server_config = ServerConfig::Gcp {
            bucket,
            credential_path,
            encryption_secret,
        };
        do_sync(&mut env, replica_ptr, "nativeSyncGcp", server_config);
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeSyncAwsAccessKey(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    region: JString,
    bucket: JString,
    access_key_id: JString,
    secret_access_key: JString,
    encryption_secret: JString,
) {
    catch_panics!(&mut env, "nativeSyncAwsAccessKey", (), {
        let region = match read_jstring(&mut env, &region, "region") { Some(s) => s, None => return };
        let bucket = match read_jstring(&mut env, &bucket, "bucket") { Some(s) => s, None => return };
        let access_key_id = match read_jstring(&mut env, &access_key_id, "accessKeyId") { Some(s) => s, None => return };
        let secret_access_key = match read_jstring(&mut env, &secret_access_key, "secretAccessKey") { Some(s) => s, None => return };
        let encryption_secret_str = match read_jstring(&mut env, &encryption_secret, "encryptionSecret") { Some(s) => s, None => return };
        let encryption_secret = match parse_encryption_secret(&mut env, &encryption_secret_str) {
            Some(b) => b,
            None => return,
        };

        let server_config = ServerConfig::Aws {
            region,
            bucket,
            credentials: AwsCredentials::AccessKey { access_key_id, secret_access_key },
            encryption_secret,
        };
        do_sync(&mut env, replica_ptr, "nativeSyncAwsAccessKey", server_config);
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeSyncAwsProfile(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    region: JString,
    bucket: JString,
    profile_name: JString,
    encryption_secret: JString,
) {
    catch_panics!(&mut env, "nativeSyncAwsProfile", (), {
        let region = match read_jstring(&mut env, &region, "region") { Some(s) => s, None => return };
        let bucket = match read_jstring(&mut env, &bucket, "bucket") { Some(s) => s, None => return };
        let profile_name = match read_jstring(&mut env, &profile_name, "profileName") { Some(s) => s, None => return };
        let encryption_secret_str = match read_jstring(&mut env, &encryption_secret, "encryptionSecret") { Some(s) => s, None => return };
        let encryption_secret = match parse_encryption_secret(&mut env, &encryption_secret_str) {
            Some(b) => b,
            None => return,
        };

        let server_config = ServerConfig::Aws {
            region,
            bucket,
            credentials: AwsCredentials::Profile { profile_name },
            encryption_secret,
        };
        do_sync(&mut env, replica_ptr, "nativeSyncAwsProfile", server_config);
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeSyncAwsDefault(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    region: JString,
    bucket: JString,
    encryption_secret: JString,
) {
    catch_panics!(&mut env, "nativeSyncAwsDefault", (), {
        let region = match read_jstring(&mut env, &region, "region") { Some(s) => s, None => return };
        let bucket = match read_jstring(&mut env, &bucket, "bucket") { Some(s) => s, None => return };
        let encryption_secret_str = match read_jstring(&mut env, &encryption_secret, "encryptionSecret") { Some(s) => s, None => return };
        let encryption_secret = match parse_encryption_secret(&mut env, &encryption_secret_str) {
            Some(b) => b,
            None => return,
        };

        let server_config = ServerConfig::Aws {
            region,
            bucket,
            credentials: AwsCredentials::Default,
            encryption_secret,
        };
        do_sync(&mut env, replica_ptr, "nativeSyncAwsDefault", server_config);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_replica() -> (Replica, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let storage_config = StorageConfig::OnDisk {
            taskdb_dir: temp_dir.path().to_path_buf(),
            create_if_missing: true,
            access_mode: taskchampion::storage::AccessMode::ReadWrite,
        };
        
        let storage = storage_config.into_storage().expect("Failed to create storage");
        let replica = Replica::new(storage);
        (replica, temp_dir)
    }

    #[test]
    fn test_replica_lifecycle() {
        let (replica, _temp_dir) = create_test_replica();
        let boxed_replica = Box::new(replica);
        let replica_ptr = Box::into_raw(boxed_replica) as jlong;
        
        assert_ne!(replica_ptr, 0);
        
        // Clean up
        unsafe {
            let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
            drop(boxed_replica);
        }
    }

    #[test]
    fn test_task_creation_and_modification() {
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();
        
        // Test task creation
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        
        // Test setting description
        task.set_description("Test task description".to_string(), &mut ops)
            .expect("Failed to set description");
        
        // Test setting status
        task.set_status(Status::Pending, &mut ops)
            .expect("Failed to set status");
        
        // Test setting custom value
        task.set_value("project", Some("test_project".to_string()), &mut ops)
            .expect("Failed to set custom value");
        
        // Commit operations
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Verify task was created and modified
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        assert_eq!(retrieved_task.get_description(), "Test task description");
        assert_eq!(retrieved_task.get_status(), Status::Pending);
        assert_eq!(retrieved_task.get_value("project"), Some("test_project"));
    }

    #[test]
    fn test_tag_operations() {
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();
        
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        
        // Test adding tag
        let tag = Tag::try_from("work").expect("Failed to create tag");
        task.add_tag(&tag, &mut ops).expect("Failed to add tag");
        
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Verify tag was added
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        let tags: Vec<_> = retrieved_task.get_tags().collect();
        assert!(tags.contains(&tag));
        
        // Test removing tag
        let mut ops = Operations::new();
        let mut task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        task.remove_tag(&tag, &mut ops).expect("Failed to remove tag");
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Verify tag was removed
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        let tags: Vec<_> = retrieved_task.get_tags().collect();
        assert!(!tags.contains(&tag));
    }

    #[test]
    fn test_annotation_operations() {
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();
        
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        
        // Test adding annotation
        let annotation = Annotation {
            entry: Utc::now(),
            description: "Test annotation".to_string(),
        };
        let entry_time = annotation.entry;
        
        task.add_annotation(annotation, &mut ops).expect("Failed to add annotation");
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Verify annotation was added
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        let annotations: Vec<_> = retrieved_task.get_annotations().collect();
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].description, "Test annotation");
        
        // Test removing annotation
        let mut ops = Operations::new();
        let mut task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        task.remove_annotation(entry_time, &mut ops).expect("Failed to remove annotation");
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Verify annotation was removed
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        
        let annotations: Vec<_> = retrieved_task.get_annotations().collect();
        assert_eq!(annotations.len(), 0);
    }

    #[test]
    fn test_undo_operations() {
        let (mut replica, _temp_dir) = create_test_replica();
        
        // Add an undo point
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        replica.commit_operations(ops).expect("Failed to add undo point");
        
        // Create a task
        let task_uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        task.set_description("Test task".to_string(), &mut ops).expect("Failed to set description");
        replica.commit_operations(ops).expect("Failed to commit task creation");
        
        // Verify task exists
        assert!(replica.get_task(task_uuid).expect("Failed to get task").is_some());
        
        // Perform undo
        let undo_ops = replica.get_undo_operations().expect("Failed to get undo operations");
        assert!(!undo_ops.is_empty());
        
        let success = replica.commit_reversed_operations(undo_ops)
            .expect("Failed to commit undo operations");
        assert!(success);
        
        // Verify task no longer exists (or is in initial state)
        // Note: The exact behavior depends on how TaskChampion handles undo
        // This test mainly verifies the undo mechanism works without errors
    }

    #[test]
    fn test_data_export() {
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();
        
        // Create a task with some data
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        task.set_description("Export test task".to_string(), &mut ops).expect("Failed to set description");
        task.set_status(Status::Pending, &mut ops).expect("Failed to set status");
        task.set_value("project", Some("export_test".to_string()), &mut ops).expect("Failed to set project");
        replica.commit_operations(ops).expect("Failed to commit operations");
        
        // Test getting all task UUIDs
        let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
        assert!(all_tasks.contains_key(&task_uuid));
        
        // Test getting task data
        let task_data = replica.get_task_data(task_uuid)
            .expect("Failed to get task data")
            .expect("Task data not found");
        
        // Verify task data contains expected fields
        assert!(task_data.get("description").is_some());
        assert!(task_data.get("status").is_some());
        assert!(task_data.get("project").is_some());
    }

    #[test]
    fn test_concurrent_task_operations() {
        use std::sync::Arc;
        use std::thread;
        
        let (replica, _temp_dir) = create_test_replica();
        let boxed_replica = Box::new(replica);
        let replica_ptr = Box::into_raw(boxed_replica) as jlong;
        
        // Register the replica mutex
        REPLICA_LOCKS.insert(replica_ptr, Arc::new(Mutex::new(())));
        
        let num_threads = 4;
        let tasks_per_thread = 10;
        let mut handles = vec![];
        
        for thread_id in 0..num_threads {
            let replica_ptr_clone = replica_ptr;
            let handle = thread::spawn(move || {
                for i in 0..tasks_per_thread {
                    let lock_arc = REPLICA_LOCKS.get(&replica_ptr_clone).unwrap().clone();
                    let _guard = lock_arc.lock().unwrap_or_else(|p| p.into_inner());
                    unsafe {
                        let replica = &mut *(replica_ptr_clone as *mut Replica);
                        let task_uuid = Uuid::new_v4();
                        let mut ops = Operations::new();

                        let mut task = replica.create_task(task_uuid, &mut ops)
                            .expect("Failed to create task");

                        task.set_description(format!("Thread {} Task {}", thread_id, i), &mut ops)
                            .expect("Failed to set description");

                        replica.commit_operations(ops)
                            .expect("Failed to commit operations");
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        // Verify all tasks were created
        {
            let lock_arc = REPLICA_LOCKS.get(&replica_ptr).unwrap().clone();
            let _guard = lock_arc.lock().unwrap_or_else(|p| p.into_inner());
            unsafe {
                let replica = &mut *(replica_ptr as *mut Replica);
                let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
                assert_eq!(all_tasks.len(), num_threads * tasks_per_thread);
            }
        }
        
        // Clean up
        REPLICA_LOCKS.remove(&replica_ptr);
        unsafe {
            let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
            drop(boxed_replica);
        }
    }

    #[test]
    fn test_timeout_handling() {
        use std::sync::Arc;
        use std::thread;
        
        let (replica, _temp_dir) = create_test_replica();
        let boxed_replica = Box::new(replica);
        let replica_ptr = Box::into_raw(boxed_replica) as jlong;
        
        // Register the replica mutex
        REPLICA_LOCKS.insert(replica_ptr, Arc::new(Mutex::new(())));
        
        // Test that poisoned mutex is recovered from
        let lock_arc = REPLICA_LOCKS.get(&replica_ptr).unwrap().clone();
        
        // Simulate a thread panic while holding the lock to poison it
        let handle = thread::spawn(move || {
            let _lock = lock_arc.lock().unwrap();
            panic!("Simulated panic to poison mutex");
        });
        
        // Wait for thread to panic
        let _ = handle.join();
        
        // Now try to use the poisoned mutex - it should recover
        {
            let lock_arc = REPLICA_LOCKS.get(&replica_ptr).unwrap().clone();
            let _guard = lock_arc.lock().unwrap_or_else(|p| p.into_inner());
            unsafe {
                let replica = &mut *(replica_ptr as *mut Replica);
                let task_uuid = Uuid::new_v4();
                let mut ops = Operations::new();

                let mut task = replica.create_task(task_uuid, &mut ops)
                    .expect("Failed to create task after mutex recovery");

                task.set_description("Test after recovery".to_string(), &mut ops)
                    .expect("Failed to set description");

                replica.commit_operations(ops)
                    .expect("Failed to commit operations");
            }
        }
        
        // Clean up
        REPLICA_LOCKS.remove(&replica_ptr);
        unsafe {
            let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
            drop(boxed_replica);
        }
    }

    #[test]
    fn test_replica_cleanup() {
        let (replica, _temp_dir) = create_test_replica();
        let boxed_replica = Box::new(replica);
        let replica_ptr = Box::into_raw(boxed_replica) as jlong;
        
        // Register the replica mutex
        REPLICA_LOCKS.insert(replica_ptr, Arc::new(Mutex::new(())));
        
        // Verify mutex is registered
        assert!(REPLICA_LOCKS.contains_key(&replica_ptr));
        
        // Clean up (simulating nativeDestroy)
        if let Some((_, _)) = REPLICA_LOCKS.remove(&replica_ptr) {
            // Mutex removed successfully
        }
        
        // Verify mutex is removed
        assert!(!REPLICA_LOCKS.contains_key(&replica_ptr));
        
        // Clean up replica
        unsafe {
            let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
            drop(boxed_replica);
        }
    }
}