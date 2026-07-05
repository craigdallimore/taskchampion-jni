use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobjectArray};
use jni::JNIEnv;
use taskchampion::{Replica, StorageConfig, Operations, Operation, Status, Tag, Annotation, ServerConfig, Task};
use taskchampion::server::AwsCredentials;
use uuid::Uuid;
use chrono::Utc;
use log::{info, error, warn};
use serde_json;
use std::env;
use std::panic;
use std::sync::atomic::{AtomicI64, Ordering};
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

// Replica handle registry.
//
// The `jlong` handles returned to Java are opaque identifiers allocated
// from a monotonically increasing counter — never memory addresses.
// Handle 0 is the invalid/failure sentinel and is never allocated, and
// handles are never reused, so a stale handle from a destroyed replica
// can never collide with a later replica (no ABA hazard).
//
// SAFETY PROPERTY: an operation can never observe a freed Replica. It
// either finds the map entry — in which case its cloned Arc keeps the
// Replica alive for the duration of the operation — or the entry is
// absent and it throws InvalidReplicaException. nativeDestroy only
// removes the map entry and never dereferences the replica; the Replica
// itself is dropped when the last Arc holder (destroy or an in-flight
// operation) finishes.
lazy_static! {
    static ref REPLICAS: DashMap<jlong, Arc<Mutex<SendReplica>>> = DashMap::new();
}

/// Newtype marking `Replica` as `Send` so it can live in the global
/// registry and be used from whichever JVM thread makes the JNI call.
///
/// SAFETY: `Replica` is `!Send` only because taskchampion's
/// `Box<dyn Storage>` trait object erases auto traits. Both concrete
/// storages produced by `StorageConfig::into_storage` are `Send`:
/// `SqliteStorage` holds a `rusqlite::Connection` (`unsafe impl Send`
/// in rusqlite), and `InMemoryStorage` holds plain owned data. The
/// per-replica `Mutex` additionally serialises all access, so the
/// replica is only ever used by one thread at a time. (The previous
/// raw-pointer scheme relied on the same property implicitly by
/// dereferencing the replica from arbitrary JVM threads.)
struct SendReplica(Replica);
unsafe impl Send for SendReplica {}

/// Next handle to allocate. Starts at 1; 0 is reserved as the failure
/// sentinel returned by nativeInitialize on error.
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// Register a Replica in the registry and return its newly allocated
/// opaque handle.
fn register_replica(replica: Replica) -> jlong {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    REPLICAS.insert(handle, Arc::new(Mutex::new(SendReplica(replica))));
    handle
}

/// Non-JNI core of `run_with_replica`: look up the handle, lock the
/// per-replica mutex, and run the closure with exclusive access to the
/// Replica. Returns `None` if the handle is not registered (never was,
/// or already destroyed). A poisoned mutex is recovered via
/// `into_inner`.
fn with_registered_replica<F, R>(handle: jlong, method_name: &str, f: F) -> Option<R>
where
    F: FnOnce(&mut Replica) -> R,
{
    // Clone the Arc and drop the DashMap ref guard *before* locking, so
    // no shard guard is held across the (potentially long) mutex
    // acquisition. From this point the cloned Arc alone keeps the
    // Replica alive, even if nativeDestroy removes the entry
    // concurrently.
    let replica_arc = {
        let entry = REPLICAS.get(&handle)?;
        Arc::clone(entry.value())
    };
    let mut guard = match replica_arc.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Replica mutex poisoned in {}, recovering", method_name);
            poisoned.into_inner()
        }
    };
    Some(f(&mut guard.0))
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
/// - If `replica_ptr` is 0 or no longer registered, throws
///   InvalidReplicaException and returns `None`.
/// - If the closure returns `Err(msg)`, throws TaskChampionStorageException
///   with that message and returns `None`.
/// - If the closure returns `Ok(value)`, returns `Some(value)`.
///
/// `None` means a Java exception is now PENDING on `env`. The caller
/// must return its sentinel to the JVM immediately without making any
/// further JNI env call (except the Exception* family): any other env
/// call with a pending exception is a JNI spec violation that aborts
/// the process on Android ("FindClass called with pending exception").
///
/// All JNI marshalling of inputs and outputs should happen outside this
/// function so the lock is held only for the replica work itself.
#[must_use]
fn run_with_replica<'local, F, R>(
    env: &mut JNIEnv<'local>,
    replica_ptr: jlong,
    method_name: &str,
    f: F,
) -> Option<R>
where
    F: FnOnce(&mut Replica) -> Result<R, String>,
{
    if replica_ptr == 0 {
        throw(
            env,
            EXC_INVALID_REPLICA,
            &format!("Null replica handle in {}", method_name),
        );
        return None;
    }

    let result = match with_registered_replica(replica_ptr, method_name, f) {
        Some(result) => result,
        None => {
            throw(
                env,
                EXC_INVALID_REPLICA,
                &format!(
                    "Invalid replica handle in {} (not registered or already destroyed)",
                    method_name
                ),
            );
            return None;
        }
    };

    match result {
        Ok(value) => Some(value),
        Err(msg) => {
            error!("{}", msg);
            throw(env, EXC_STORAGE, &msg);
            None
        }
    }
}



// Helper function to create string array. Must only be called with no
// exception pending. On JNI failure, returns null after ensuring an
// exception is pending (either the one the failed JNI call raised, e.g.
// OutOfMemoryError, or a TaskChampionStorageException thrown here) and
// makes no further env calls — building a fallback array at that point
// would abort the process.
fn create_string_array<'local>(env: &mut JNIEnv<'local>, strings: Vec<String>) -> jobjectArray {
    let string_class = match env.find_class("java/lang/String") {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to find String class: {:?}", e);
            throw(env, EXC_STORAGE, &format!("Failed to marshal string array: {}", e));
            return std::ptr::null_mut();
        }
    };
    let java_array = match env.new_object_array(strings.len() as i32, &string_class, JObject::null()) {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to create Java array: {:?}", e);
            throw(env, EXC_STORAGE, &format!("Failed to marshal string array: {}", e));
            return std::ptr::null_mut();
        }
    };
    for (i, s) in strings.iter().enumerate() {
        let java_string = match env.new_string(s) {
            Ok(js) => js,
            Err(e) => {
                error!("Failed to create Java string: {:?}", e);
                throw(env, EXC_STORAGE, &format!("Failed to marshal string array: {}", e));
                return std::ptr::null_mut();
            }
        };
        if let Err(e) = env.set_object_array_element(&java_array, i as i32, java_string) {
            error!("Failed to set array element {}: {:?}", i, e);
            throw(env, EXC_STORAGE, &format!("Failed to marshal string array: {}", e));
            return std::ptr::null_mut();
        }
    }
    java_array.into_raw()
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
        let handle = register_replica(replica);

        info!("Replica initialized successfully, handle: {}", handle);
        handle
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
            throw(&mut env, EXC_INVALID_REPLICA, "Cannot destroy a null replica handle");
            return;
        }

        info!("Destroying Replica with handle: {}", replica_ptr);

        // Removing the entry drops this registry's Arc. In-flight
        // operations hold their own Arc clones, so the Replica is freed
        // only when the last holder finishes; destroy itself never
        // dereferences the replica. Any subsequent call with this handle
        // finds no entry and throws InvalidReplicaException.
        match REPLICAS.remove(&replica_ptr) {
            Some(_) => {
                info!("Replica handle {} destroyed successfully", replica_ptr);
            }
            None => {
                throw(
                    &mut env,
                    EXC_INVALID_REPLICA,
                    &format!("Replica handle {} is not registered (already destroyed?)", replica_ptr),
                );
            }
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

        let outcome = run_with_replica(&mut env, replica_ptr, "nativeUndo", |replica| {
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

        let Some(outcome) = outcome else {
            // Exception pending; return the sentinel without further env calls.
            return 0;
        };

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
        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeAddUndoPoint", |replica| {
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
        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeRebuildWorkingSet", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeCreateTask", |replica| {
            let mut ops = Operations::new();
            replica
                .create_task(task_uuid, &mut ops)
                .map_err(|e| format!("Failed to create task: {}", e))?;
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskSetDescription", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskSetStatus", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskSetValue", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskAddTag", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskRemoveTag", |replica| {
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

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskAddAnnotation", |replica| {
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
                    EXC_STORAGE,
                    &format!("Invalid annotation entry timestamp: {}", entry_timestamp),
                );
                return;
            }
        };

        // On None an exception is pending; nothing further touches env.
        let _ = run_with_replica(&mut env, replica_ptr, "nativeTaskRemoveAnnotation", |replica| {
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

/// Build the JSON document for a single task, in the schema documented on
/// nativeGetTaskData: uuid, description?, status?, entry?, modified?,
/// tags[], annotations[{entry, description}], udas{}.
///
/// Well-known fields are plucked from the raw key/value map; keys starting
/// with `tag_` and `annotation_` (taskchampion's structural encoding) are
/// skipped in favour of the tags/annotations arrays; everything else is
/// routed to udas.
fn task_to_json(uuid_str: &str, task: &Task) -> Result<String, String> {
    use serde_json::{json, Map, Value};

    let mut udas = Map::new();
    let mut description: Option<Value> = None;
    let mut status: Option<Value> = None;
    let mut entry: Option<Value> = None;
    let mut modified: Option<Value> = None;

    // Task::get_taskmap is deprecated in favour of TaskData::properties,
    // but TaskData is only reachable by consuming the Task
    // (into_task_data), and no other &Task accessor enumerates the raw
    // key/value map. Its content is identical to what
    // Replica::get_task_data returns for the same task.
    #[allow(deprecated)]
    let taskmap = task.get_taskmap();
    for (key, value) in taskmap.iter() {
        match key.as_str() {
            "description" => description = Some(Value::String(value.clone())),
            "status" => status = Some(Value::String(value.clone())),
            "entry" => entry = Some(Value::String(value.clone())),
            "modified" => modified = Some(Value::String(value.clone())),
            k if k.starts_with("tag_") => {} // exposed via the tags array
            k if k.starts_with("annotation_") => {} // exposed via the annotations array
            _ => {
                udas.insert(key.clone(), Value::String(value.clone()));
            }
        }
    }

    let tags_array: Vec<Value> = task
        .get_tags()
        .map(|t| Value::String(t.to_string()))
        .collect();

    let annotations_array: Vec<Value> = task
        .get_annotations()
        .map(|a| json!({
            "entry": a.entry.timestamp().to_string(),
            "description": a.description,
        }))
        .collect();

    let mut root = Map::new();
    root.insert("uuid".to_string(), Value::String(uuid_str.to_string()));
    if let Some(v) = description { root.insert("description".to_string(), v); }
    if let Some(v) = status { root.insert("status".to_string(), v); }
    if let Some(v) = entry { root.insert("entry".to_string(), v); }
    if let Some(v) = modified { root.insert("modified".to_string(), v); }
    root.insert("tags".to_string(), Value::Array(tags_array));
    root.insert("annotations".to_string(), Value::Array(annotations_array));
    root.insert("udas".to_string(), Value::Object(udas));

    serde_json::to_string(&Value::Object(root))
        .map_err(|e| format!("Failed to serialize task data to JSON: {}", e))
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetAllTaskUuids<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
) -> jobjectArray {
    catch_panics!(&mut env, "nativeGetAllTaskUuids", std::ptr::null_mut(), {
        let task_uuids = run_with_replica(&mut env, replica_ptr, "nativeGetAllTaskUuids", |replica| {
            let tasks = replica
                .all_tasks()
                .map_err(|e| format!("Failed to get all tasks: {}", e))?;
            info!("Found {} task UUIDs", tasks.len());
            Ok(tasks.keys().map(|uuid| uuid.to_string()).collect::<Vec<String>>())
        });

        let Some(task_uuids) = task_uuids else {
            // Exception pending; any further env call would abort the process.
            return std::ptr::null_mut();
        };

        create_string_array(&mut env, task_uuids)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetAllTasks<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
) -> jobjectArray {
    catch_panics!(&mut env, "nativeGetAllTasks", std::ptr::null_mut(), {
        let task_docs = run_with_replica(&mut env, replica_ptr, "nativeGetAllTasks", |replica| {
            let tasks = replica
                .all_tasks()
                .map_err(|e| format!("Failed to get all tasks: {}", e))?;
            let mut docs = Vec::with_capacity(tasks.len());
            for (uuid, task) in tasks.iter() {
                docs.push(task_to_json(&uuid.to_string(), task)?);
            }
            info!("Retrieved {} tasks", docs.len());
            Ok(docs)
        });

        let Some(task_docs) = task_docs else {
            // Exception pending; any further env call would abort the process.
            return std::ptr::null_mut();
        };

        create_string_array(&mut env, task_docs)
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

    // Inner None signals "task not found" — returned to Java as null.
    // Outer None signals a thrown exception. Storage errors throw.
    let json_result: Option<Option<String>> = run_with_replica(&mut env, replica_ptr, "nativeGetTaskData", |replica| {
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

        let json = task_to_json(&uuid_str, &task)?;
        info!("Retrieved task data for: {}", uuid_str);
        Ok(Some(json))
    });

    let Some(json_result) = json_result else {
        // Exception pending; any further env call would abort the process.
        return JObject::null().into();
    };

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
    // Inner None signals "no task at this index" — returned to Java as
    // null. Outer None signals a thrown exception.
    let uuid_string: Option<Option<String>> = run_with_replica(&mut env, replica_ptr, "nativeGetUuidForIndex", |replica| {
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

    let Some(uuid_string) = uuid_string else {
        // Exception pending; any further env call would abort the process.
        return JObject::null().into();
    };

    match uuid_string {
        Some(s) => match env.new_string(s) {
            Ok(jstr) => jstr,
            Err(e) => {
                error!("Failed to create JString for UUID: {:?}", e);
                throw(&mut env, EXC_STORAGE, &format!("Failed to marshal UUID: {}", e));
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
        PostSyncRebuild(String),
    }

    let result: Option<Result<(), SyncFailure>> = run_with_replica(
        env,
        replica_ptr,
        method_name,
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
                    match replica.rebuild_working_set(true) {
                        Ok(()) => {
                            info!("Working set rebuilt after sync");
                            Ok(Ok(()))
                        }
                        Err(e) => Ok(Err(SyncFailure::PostSyncRebuild(format!("{}", e)))),
                    }
                }
                Ok(Err(e)) => Ok(Err(SyncFailure::Failed(format!("{}", e)))),
                Err(panic_err) => {
                    error!("Sync operation panicked (likely TLS certificate issue): {:?}", panic_err);
                    Ok(Err(SyncFailure::TlsPanic))
                }
            }
        },
    );

    let Some(result) = result else {
        // InvalidReplicaException is pending; the `throw` helper below
        // would no-op anyway, but return early for clarity.
        return;
    };

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
        Err(SyncFailure::PostSyncRebuild(msg)) => {
            error!("Failed to rebuild working set after sync: {}", msg);
            throw(
                env,
                EXC_STORAGE,
                &format!(
                    "Sync succeeded but post-sync working-set rebuild failed: {}",
                    msg
                ),
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
        if bucket.is_empty() {
            throw(&mut env, EXC_SYNC, "bucket must not be empty");
            return;
        }
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
        if bucket.is_empty() {
            throw(&mut env, EXC_SYNC, "bucket must not be empty");
            return;
        }
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
        if bucket.is_empty() {
            throw(&mut env, EXC_SYNC, "bucket must not be empty");
            return;
        }
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
        if bucket.is_empty() {
            throw(&mut env, EXC_SYNC, "bucket must not be empty");
            return;
        }
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
        let handle = register_replica(replica);

        // 0 is the failure sentinel and must never be allocated.
        assert_ne!(handle, 0);
        assert!(REPLICAS.contains_key(&handle));

        // Clean up (simulating nativeDestroy): removing the entry drops
        // the registry's Arc, and with no other holders the Replica drops.
        assert!(REPLICAS.remove(&handle).is_some());
        assert!(!REPLICAS.contains_key(&handle));
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
    fn test_create_task_fresh_uuid_commits_empty_task() {
        // The bare create path (create_task + commit, no field writes) must
        // persist an empty task: no description, status, entry, or modified.
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();

        let mut ops = Operations::new();
        replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        replica.commit_operations(ops).expect("Failed to commit operations");

        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found after commit");
        assert_eq!(retrieved_task.get_description(), "");
        assert_eq!(retrieved_task.get_value("status"), None);
        assert_eq!(retrieved_task.get_value("entry"), None);
        assert_eq!(retrieved_task.get_value("modified"), None);
    }

    #[test]
    fn test_create_task_existing_uuid_is_noop() {
        // Mirrors nativeCreateTask's get-or-create contract: calling the
        // create path with an existing UUID must leave the task untouched.
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();

        // First create, then set some values (as a real caller would).
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        task.set_description("Original description".to_string(), &mut ops)
            .expect("Failed to set description");
        task.set_value("entry", Some("1700000000".to_string()), &mut ops)
            .expect("Failed to set entry");
        replica.commit_operations(ops).expect("Failed to commit operations");

        // Second create with the same UUID — the create path as implemented
        // in nativeCreateTask: create_task followed by commit_operations.
        let mut ops = Operations::new();
        replica.create_task(task_uuid, &mut ops).expect("Failed on second create");
        assert!(ops.is_empty(), "Existing UUID should push no operations");
        replica.commit_operations(ops).expect("Failed to commit operations");

        // Stored values are unchanged and no duplicate task appeared.
        let retrieved_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        assert_eq!(retrieved_task.get_description(), "Original description");
        assert_eq!(retrieved_task.get_value("entry"), Some("1700000000"));
        assert_eq!(replica.all_tasks().expect("Failed to get all tasks").len(), 1);
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
        use std::thread;

        let (replica, _temp_dir) = create_test_replica();
        let handle = register_replica(replica);

        let num_threads = 4;
        let tasks_per_thread = 10;
        let mut join_handles = vec![];

        for thread_id in 0..num_threads {
            let join_handle = thread::spawn(move || {
                for i in 0..tasks_per_thread {
                    with_registered_replica(handle, "test_concurrent_task_operations", |replica| {
                        let task_uuid = Uuid::new_v4();
                        let mut ops = Operations::new();

                        let mut task = replica.create_task(task_uuid, &mut ops)
                            .expect("Failed to create task");

                        task.set_description(format!("Thread {} Task {}", thread_id, i), &mut ops)
                            .expect("Failed to set description");

                        replica.commit_operations(ops)
                            .expect("Failed to commit operations");
                    })
                    .expect("Handle should remain registered for the whole test");
                }
            });
            join_handles.push(join_handle);
        }

        for join_handle in join_handles {
            join_handle.join().expect("Thread panicked");
        }

        // Verify all tasks were created
        let total = with_registered_replica(handle, "test_concurrent_task_operations", |replica| {
            replica.all_tasks().expect("Failed to get all tasks").len()
        })
        .expect("Handle should remain registered for the whole test");
        assert_eq!(total, num_threads * tasks_per_thread);

        // Clean up
        assert!(REPLICAS.remove(&handle).is_some());
    }

    #[test]
    fn test_timeout_handling() {
        use std::thread;

        let (replica, _temp_dir) = create_test_replica();
        let handle = register_replica(replica);

        // Test that a poisoned mutex is recovered from: panic while
        // holding the per-replica lock.
        let replica_arc = Arc::clone(REPLICAS.get(&handle).unwrap().value());
        let join_handle = thread::spawn(move || {
            let _lock = replica_arc.lock().unwrap();
            panic!("Simulated panic to poison mutex");
        });

        // Wait for thread to panic
        let _ = join_handle.join();

        // Now use the registry path against the poisoned mutex - it
        // should recover.
        with_registered_replica(handle, "test_timeout_handling", |replica| {
            let task_uuid = Uuid::new_v4();
            let mut ops = Operations::new();

            let mut task = replica.create_task(task_uuid, &mut ops)
                .expect("Failed to create task after mutex recovery");

            task.set_description("Test after recovery".to_string(), &mut ops)
                .expect("Failed to set description");

            replica.commit_operations(ops)
                .expect("Failed to commit operations");
        })
        .expect("Handle should remain registered for the whole test");

        // Clean up
        assert!(REPLICAS.remove(&handle).is_some());
    }

    #[test]
    fn test_replica_cleanup() {
        let (replica, _temp_dir) = create_test_replica();
        let handle = register_replica(replica);

        // Verify the handle is registered
        assert!(REPLICAS.contains_key(&handle));

        // Clean up (simulating nativeDestroy)
        assert!(REPLICAS.remove(&handle).is_some());

        // Verify the handle is removed; a second destroy finds nothing
        // (nativeDestroy throws InvalidReplicaException in that case).
        assert!(!REPLICAS.contains_key(&handle));
        assert!(REPLICAS.remove(&handle).is_none());

        // Operations against the destroyed handle fail cleanly with None
        // (run_with_replica throws InvalidReplicaException in that case).
        assert!(with_registered_replica(handle, "test_replica_cleanup", |_| ()).is_none());
    }

    #[test]
    fn test_destroy_during_operation() {
        use std::sync::atomic::AtomicBool;
        use std::thread;

        let (replica, _temp_dir) = create_test_replica();
        let handle = register_replica(replica);

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);

        // Worker: hammer the registry path until the handle disappears
        // (or the test tells it to stop).
        let worker = thread::spawn(move || {
            let mut completed = 0u32;
            while !worker_stop.load(Ordering::Relaxed) {
                let outcome =
                    with_registered_replica(handle, "test_destroy_during_operation", |replica| {
                        let task_uuid = Uuid::new_v4();
                        let mut ops = Operations::new();
                        replica.create_task(task_uuid, &mut ops)
                            .expect("Failed to create task");
                        replica.commit_operations(ops)
                            .expect("Failed to commit operations");
                    });
                match outcome {
                    Some(()) => completed += 1,
                    // Handle destroyed mid-loop: the clean failure path
                    // (a JNI caller would see InvalidReplicaException).
                    None => break,
                }
            }
            completed
        });

        // Let the worker run some operations, then destroy the handle
        // while the worker may be mid-operation. remove() never
        // dereferences the replica; the worker's cloned Arc keeps it
        // alive until any in-flight operation completes.
        thread::sleep(std::time::Duration::from_millis(20));
        assert!(REPLICAS.remove(&handle).is_some());
        stop.store(true, Ordering::Relaxed);

        // No crash/UB: the worker either completed operations or bailed
        // out cleanly when the handle vanished.
        let _completed = worker.join().expect("Worker thread panicked");

        // Post-destroy lookups find nothing.
        assert!(REPLICAS.get(&handle).is_none());
        assert!(
            with_registered_replica(handle, "test_destroy_during_operation", |_| ()).is_none()
        );
    }

    #[test]
    fn test_stale_handle_after_reinitialize() {
        // Destroying handle A and then initializing a new replica B must
        // never let A resolve to B: handles come from a monotonic counter
        // and are never reused, so the ABA hazard of address-based
        // handles cannot occur.
        let (replica_a, _temp_dir_a) = create_test_replica();
        let handle_a = register_replica(replica_a);
        assert!(REPLICAS.remove(&handle_a).is_some());

        let (replica_b, _temp_dir_b) = create_test_replica();
        let handle_b = register_replica(replica_b);

        assert_ne!(handle_a, handle_b, "Handles must never be reused");
        assert!(
            with_registered_replica(handle_a, "test_stale_handle_after_reinitialize", |_| ())
                .is_none(),
            "Stale handle A must not resolve to any replica"
        );

        // Handle B still resolves normally.
        with_registered_replica(handle_b, "test_stale_handle_after_reinitialize", |replica| {
            replica.all_tasks().expect("Failed to get all tasks");
        })
        .expect("Handle B should resolve");

        assert!(REPLICAS.remove(&handle_b).is_some());
    }

    #[test]
    fn test_task_to_json_bulk_round_trip() {
        let (mut replica, _temp_dir) = create_test_replica();

        // Create two tasks with descriptions, status, tags, annotations,
        // UDAs, and explicit entry/modified timestamps.
        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();

        let mut ops = Operations::new();
        let mut task_a = replica.create_task(uuid_a, &mut ops).expect("Failed to create task A");
        task_a.set_description("Task A".to_string(), &mut ops).expect("Failed to set description");
        task_a.set_status(Status::Pending, &mut ops).expect("Failed to set status");
        task_a.set_value("entry", Some("1700000000".to_string()), &mut ops).expect("Failed to set entry");
        task_a.set_value("modified", Some("1700000001".to_string()), &mut ops).expect("Failed to set modified");
        task_a.set_value("project", Some("alpha".to_string()), &mut ops).expect("Failed to set UDA");
        let tag = Tag::try_from("work").expect("Failed to create tag");
        task_a.add_tag(&tag, &mut ops).expect("Failed to add tag");
        task_a.add_annotation(
            Annotation {
                entry: chrono::DateTime::from_timestamp(1700000002, 0).unwrap(),
                description: "note on A".to_string(),
            },
            &mut ops,
        ).expect("Failed to add annotation");

        let mut task_b = replica.create_task(uuid_b, &mut ops).expect("Failed to create task B");
        task_b.set_description("Task B".to_string(), &mut ops).expect("Failed to set description");
        replica.commit_operations(ops).expect("Failed to commit operations");

        // Build JSON for every task via the bulk path (all_tasks).
        let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
        assert_eq!(all_tasks.len(), 2);

        let json_a: serde_json::Value = serde_json::from_str(
            &task_to_json(&uuid_a.to_string(), all_tasks.get(&uuid_a).expect("Task A missing"))
                .expect("Failed to build JSON for task A"),
        ).expect("Task A JSON did not parse");

        assert_eq!(json_a["uuid"], uuid_a.to_string());
        assert_eq!(json_a["description"], "Task A");
        assert_eq!(json_a["status"], "pending");
        assert_eq!(json_a["entry"], "1700000000");
        assert_eq!(json_a["modified"], "1700000001");
        // get_tags also yields taskchampion's synthetic tags (e.g.
        // PENDING, UNBLOCKED), so assert membership rather than equality.
        let tags_a: Vec<&str> = json_a["tags"]
            .as_array()
            .expect("tags is not an array")
            .iter()
            .map(|v| v.as_str().expect("tag is not a string"))
            .collect();
        assert!(tags_a.contains(&"work"), "tags {:?} missing 'work'", tags_a);
        assert_eq!(
            json_a["annotations"],
            serde_json::json!([{"entry": "1700000002", "description": "note on A"}])
        );
        // UDAs must contain exactly the custom key: no leakage of the
        // well-known fields or the tag_/annotation_ structural keys.
        assert_eq!(json_a["udas"], serde_json::json!({"project": "alpha"}));

        let json_b: serde_json::Value = serde_json::from_str(
            &task_to_json(&uuid_b.to_string(), all_tasks.get(&uuid_b).expect("Task B missing"))
                .expect("Failed to build JSON for task B"),
        ).expect("Task B JSON did not parse");

        assert_eq!(json_b["uuid"], uuid_b.to_string());
        assert_eq!(json_b["description"], "Task B");
        let tags_b: Vec<&str> = json_b["tags"]
            .as_array()
            .expect("tags is not an array")
            .iter()
            .map(|v| v.as_str().expect("tag is not a string"))
            .collect();
        assert!(!tags_b.contains(&"work"), "task B unexpectedly tagged 'work'");
        assert_eq!(json_b["annotations"], serde_json::json!([]));
        assert_eq!(json_b["udas"], serde_json::json!({}));
    }

    #[test]
    fn test_task_to_json_single_and_bulk_paths_match() {
        let (mut replica, _temp_dir) = create_test_replica();
        let task_uuid = Uuid::new_v4();

        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).expect("Failed to create task");
        task.set_description("Compare paths".to_string(), &mut ops).expect("Failed to set description");
        task.set_status(Status::Completed, &mut ops).expect("Failed to set status");
        task.set_value("priority", Some("H".to_string()), &mut ops).expect("Failed to set UDA");
        let tag = Tag::try_from("home").expect("Failed to create tag");
        task.add_tag(&tag, &mut ops).expect("Failed to add tag");
        task.add_annotation(
            Annotation {
                entry: chrono::DateTime::from_timestamp(1700000100, 0).unwrap(),
                description: "compare note".to_string(),
            },
            &mut ops,
        ).expect("Failed to add annotation");
        replica.commit_operations(ops).expect("Failed to commit operations");

        // Single-task path (as used by nativeGetTaskData).
        let single_task = replica.get_task(task_uuid)
            .expect("Failed to get task")
            .expect("Task not found");
        let single_json = task_to_json(&task_uuid.to_string(), &single_task)
            .expect("Failed to build JSON via single path");

        // Bulk path (as used by nativeGetAllTasks).
        let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
        let bulk_json = task_to_json(
            &task_uuid.to_string(),
            all_tasks.get(&task_uuid).expect("Task missing from all_tasks"),
        ).expect("Failed to build JSON via bulk path");

        // Compare as parsed values so map key ordering cannot matter.
        let single_value: serde_json::Value = serde_json::from_str(&single_json)
            .expect("Single-path JSON did not parse");
        let bulk_value: serde_json::Value = serde_json::from_str(&bulk_json)
            .expect("Bulk-path JSON did not parse");
        assert_eq!(single_value, bulk_value);
    }

    #[test]
    fn test_all_tasks_empty_replica_yields_empty_docs() {
        let (mut replica, _temp_dir) = create_test_replica();

        let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
        assert!(all_tasks.is_empty());

        // Mirror the nativeGetAllTasks closure: an empty replica must
        // produce an empty vec of JSON documents (empty Java array).
        let docs: Vec<String> = all_tasks
            .iter()
            .map(|(uuid, task)| task_to_json(&uuid.to_string(), task))
            .collect::<Result<Vec<_>, _>>()
            .expect("Failed to build docs for empty replica");
        assert!(docs.is_empty());
    }
}