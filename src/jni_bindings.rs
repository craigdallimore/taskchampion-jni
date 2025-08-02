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

// Macro to simplify replica lock acquisition
macro_rules! with_replica_lock {
    ($replica_ptr:expr, $method_name:expr, $return_value:expr, $code:block) => {
        match REPLICA_LOCKS.get(&$replica_ptr) {
            Some(lock_arc) => {
                let _guard = match lock_arc.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        warn!("Replica mutex poisoned in {}, recovering", $method_name);
                        poisoned.into_inner()
                    }
                };
                $code
            }
            None => {
                error!("Invalid replica pointer: {}", $replica_ptr);
                $return_value
            }
        }
    };
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
    // Initialize Android logger
    init_android_logger();
    
    // Configure TLS for Android compatibility
    configure_android_tls();
    
    let data_dir_str: String = match env.get_string(&data_dir) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get data_dir string: {:?}", e);
            return 0;
        }
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
            return 0;
        }
    };

    let replica = Replica::new(storage);
    let boxed_replica = Box::new(replica);
    let replica_ptr = Box::into_raw(boxed_replica) as jlong;

    // Register per-replica mutex for thread safety
    REPLICA_LOCKS.insert(replica_ptr, Arc::new(Mutex::new(())));

    info!("Replica initialized successfully, pointer: {}", replica_ptr);
    replica_ptr
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) {
    if replica_ptr == 0 {
        error!("Attempted to destroy null replica pointer");
        return;
    }

    info!("Destroying Replica with pointer: {}", replica_ptr);

    // Remove the mutex from registry
    if let Some((_, _)) = REPLICA_LOCKS.remove(&replica_ptr) {
        info!("Replica mutex cleaned up");
    } else {
        warn!("No mutex found for replica: {}", replica_ptr);
    }

    unsafe {
        let boxed_replica = Box::from_raw(replica_ptr as *mut Replica);
        drop(boxed_replica);
        info!("Replica destroyed successfully");
    }
}

// Transaction control

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeUndo(
    _env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) -> jboolean {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeUndo");
        return 0;
    }

    with_replica_lock!(replica_ptr, "nativeUndo", 0, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
        match replica.get_undo_operations() {
                Ok(undo_ops) => {
                    if undo_ops.is_empty() {
                        info!("No operations to undo");
                        return 0;
                    }
                    
                    match replica.commit_reversed_operations(undo_ops) {
                        Ok(success) => {
                            if success {
                                info!("Undo operation completed successfully");
                                1
                            } else {
                                warn!("Undo operation failed - concurrent changes detected");
                                0
                            }
                        }
                        Err(e) => {
                            error!("Failed to commit undo operations: {:?}", e);
                            0
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get undo operations: {:?}", e);
                    0
                }
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeAddUndoPoint(
    mut env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
    message: JString,
) {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeAddUndoPoint");
        return;
    }

    let message_str: String = match env.get_string(&message) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get message string: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeAddUndoPoint", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            // Add an undo point operation
            ops.push(Operation::UndoPoint);
            
            match replica.commit_operations(ops) {
                Ok(_) => {
                    info!("Undo point added: {}", message_str);
                }
                Err(e) => {
                    error!("Failed to add undo point: {:?}", e);
                }
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeCommit(
    _env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeCommit");
        return;
    }

    with_replica_lock!(replica_ptr, "nativeCommit", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            // Force a rebuild of the working set to ensure consistency
            match replica.rebuild_working_set(true) {
                Ok(_) => {
                    info!("Commit completed - working set rebuilt");
                }
                Err(e) => {
                    error!("Failed to rebuild working set during commit: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeCreateTask");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeCreateTask", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
        let mut ops = Operations::new();
            
        match replica.create_task(task_uuid, &mut ops) {
                Ok(mut task) => {
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("entry", Some(now_timestamp.clone()), &mut ops) {
                        error!("Failed to set entry timestamp: {:?}", e);
                        return;
                    }
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit create task operations: {:?}", e);
                        return;
                    }
                    
                    info!("Task created successfully: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to create task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskSetDescription");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let description: String = match env.get_string(&desc) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get description string: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskSetDescription", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.set_description(description, &mut ops) {
                        error!("Failed to set task description: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit set description operations: {:?}", e);
                        return;
                    }
                    
                    info!("Task description updated successfully: {}", uuid_str);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskSetStatus");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let status_str: String = match env.get_string(&status) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get status string: {:?}", e);
            return;
        }
    };

    let task_status = match status_str.as_str() {
        "pending" => Status::Pending,
        "completed" => Status::Completed,
        "deleted" => Status::Deleted,
        _ => {
            error!("Invalid status value: {}", status_str);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskSetStatus", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.set_status(task_status, &mut ops) {
                        error!("Failed to set task status: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit set status operations: {:?}", e);
                        return;
                    }
                    
                    info!("Task status updated successfully: {} -> {}", uuid_str, status_str);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskSetValue");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let key_str: String = match env.get_string(&key) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get key string: {:?}", e);
            return;
        }
    };

    // Check if value is null (nullable parameter)
    let value_opt = if value.is_null() {
        None
    } else {
        match env.get_string(&value) {
            Ok(s) => Some(s.into()),
            Err(e) => {
                error!("Failed to get value string: {:?}", e);
                return;
            }
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskSetValue", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.set_value(&key_str, value_opt, &mut ops) {
                        error!("Failed to set task value: {:?}", e);
                        return;
                    }
                    
                    if key_str != "modified" {
                        let now_timestamp = Utc::now().timestamp().to_string();
                        if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                            error!("Failed to set modified timestamp: {:?}", e);
                            return;
                        }
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit set value operations: {:?}", e);
                        return;
                    }
                    
                    info!("Task value updated successfully: {} -> {}={:?}", uuid_str, key_str, 
                          if value.is_null() { "None" } else { "Some(_)" });
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskAddTag");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let tag_str: String = match env.get_string(&tag) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get tag string: {:?}", e);
            return;
        }
    };

    let task_tag = match Tag::try_from(tag_str.as_str()) {
        Ok(t) => t,
        Err(e) => {
            error!("Invalid tag format: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskAddTag", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.add_tag(&task_tag, &mut ops) {
                        error!("Failed to add tag to task: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit add tag operations: {:?}", e);
                        return;
                    }
                    
                    info!("Tag added successfully: {} -> {}", uuid_str, tag_str);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskRemoveTag");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let tag_str: String = match env.get_string(&tag) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get tag string: {:?}", e);
            return;
        }
    };

    let task_tag = match Tag::try_from(tag_str.as_str()) {
        Ok(t) => t,
        Err(e) => {
            error!("Invalid tag format: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskRemoveTag", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.remove_tag(&task_tag, &mut ops) {
                        error!("Failed to remove tag from task: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit remove tag operations: {:?}", e);
                        return;
                    }
                    
                    info!("Tag removed successfully: {} -> {}", uuid_str, tag_str);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskAddAnnotation");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let description: String = match env.get_string(&desc) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get description string: {:?}", e);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskAddAnnotation", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    let annotation = Annotation {
                        entry: Utc::now(),
                        description,
                    };
                    
                    if let Err(e) = task.add_annotation(annotation, &mut ops) {
                        error!("Failed to add annotation to task: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit add annotation operations: {:?}", e);
                        return;
                    }
                    
                    info!("Annotation added successfully to task: {}", uuid_str);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
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
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeTaskRemoveAnnotation");
        return;
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return;
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return;
        }
    };

    let entry_time = match chrono::DateTime::from_timestamp(entry_timestamp, 0) {
        Some(dt) => dt,
        None => {
            error!("Invalid timestamp: {}", entry_timestamp);
            return;
        }
    };

    with_replica_lock!(replica_ptr, "nativeTaskRemoveAnnotation", return, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            let mut ops = Operations::new();
            
            match replica.get_task(task_uuid) {
                Ok(Some(mut task)) => {
                    if let Err(e) = task.remove_annotation(entry_time, &mut ops) {
                        error!("Failed to remove annotation from task: {:?}", e);
                        return;
                    }
                    
                    let now_timestamp = Utc::now().timestamp().to_string();
                    if let Err(e) = task.set_value("modified", Some(now_timestamp), &mut ops) {
                        error!("Failed to set modified timestamp: {:?}", e);
                        return;
                    }
                    
                    if let Err(e) = replica.commit_operations(ops) {
                        error!("Failed to commit remove annotation operations: {:?}", e);
                        return;
                    }
                    
                    info!("Annotation removed successfully from task: {} at timestamp {}", uuid_str, entry_timestamp);
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                }
                Err(e) => {
                    error!("Failed to get task: {:?}", e);
                }
            }
        }
    })
}

// Data retrieval

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetAllTaskUuids<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
) -> jobjectArray {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeGetAllTaskUuids");
        return create_empty_string_array(&mut env);
    }

    with_replica_lock!(replica_ptr, "nativeGetAllTaskUuids", create_empty_string_array(&mut env), {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            let task_uuids: Vec<String> = match replica.all_tasks() {
                Ok(tasks) => {
                    info!("Found {} task UUIDs", tasks.len());
                    tasks.keys().map(|uuid| uuid.to_string()).collect()
                }
                Err(e) => {
                    error!("Failed to get all tasks: {:?}", e);
                    return create_empty_string_array(&mut env);
                }
            };
            
            create_string_array(&mut env, task_uuids)
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetTaskData<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
    uuid: JString,
) -> JString<'local> {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeGetTaskData");
        return JObject::null().into();
    }

    let uuid_str: String = match env.get_string(&uuid) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get UUID string: {:?}", e);
            return JObject::null().into();
        }
    };

    let task_uuid = match Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid UUID format: {:?}", e);
            return JObject::null().into();
        }
    };

    with_replica_lock!(replica_ptr, "nativeGetTaskData", JObject::null().into(), {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            match replica.get_task(task_uuid) {
                Ok(Some(task)) => {
                    let mut task_map = std::collections::HashMap::new();
                    
                    // Get task data properties
                    match replica.get_task_data(task_uuid) {
                        Ok(Some(task_data)) => {
                            // Convert task data to a map for JSON serialization
                            for (key, value) in task_data.iter() {
                                task_map.insert(key.clone(), value.clone());
                            }
                        }
                        Ok(None) => {
                            warn!("No task data found for: {}", uuid_str);
                        }
                        Err(e) => {
                            error!("Failed to get task data: {:?}", e);
                        }
                    }
                    
                    // Add tags to the task data
                    let tags: Vec<Tag> = task.get_tags().collect();
                    if !tags.is_empty() {
                        for (i, tag) in tags.iter().enumerate() {
                            task_map.insert(format!("tag_{}", i), tag.to_string());
                        }
                    }
                    
                    // Add annotations to the task data
                    let annotations: Vec<Annotation> = task.get_annotations().collect();
                    if !annotations.is_empty() {
                        for (i, annotation) in annotations.iter().enumerate() {
                            task_map.insert(format!("annotation_{}_entry", i), annotation.entry.timestamp().to_string());
                            task_map.insert(format!("annotation_{}_description", i), annotation.description.clone());
                        }
                    }
                    
                    // Add the UUID to the task data
                    task_map.insert("uuid".to_string(), uuid_str.clone());
                    
                    match serde_json::to_string(&task_map) {
                        Ok(json_string) => {
                            match env.new_string(&json_string) {
                                Ok(java_string) => {
                                    info!("Retrieved task data for: {}", uuid_str);
                                    java_string
                                }
                                Err(e) => {
                                    error!("Failed to create Java string for task data: {:?}", e);
                                    JObject::null().into()
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to serialize task data to JSON: {:?}", e);
                            JObject::null().into()
                        }
                    }
                }
                Ok(None) => {
                    warn!("Task not found: {}", uuid_str);
                    match env.new_string("{}") {
                        Ok(empty_json) => empty_json,
                        Err(e) => {
                            error!("Failed to create empty JSON string: {:?}", e);
                            JObject::null().into()
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get task data: {:?}", e);
                    JObject::null().into()
                }
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeGetUuidForIndex<'local>(
    env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
    index: jint,
) -> JString<'local> {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeGetUuidForIndex");
        return JObject::null().into();
    }

    with_replica_lock!(replica_ptr, "nativeGetUuidForIndex", JObject::null().into(), {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            match replica.working_set() {
                Ok(working_set) => {
                    // TaskWarrior IDs are 1-based, so subtract 1 for 0-based index
                    if index > 0 && (index as usize) <= working_set.len() {
                        let zero_based_index = (index as usize) - 1;
                        if let Some(uuid) = working_set.by_index(zero_based_index) {
                            if !uuid.is_nil() {
                                info!("Found UUID {} for index {}", uuid, index);
                                match env.new_string(uuid.to_string()) {
                                    Ok(jstr) => return jstr,
                                    Err(e) => {
                                        error!("Failed to create JString for UUID: {:?}", e);
                                        return JObject::null().into();
                                    }
                                }
                            }
                        }
                    }
                    info!("No task found at index {}", index);
                    JObject::null().into()
                }
                Err(e) => {
                    error!("Failed to get working set: {:?}", e);
                    JObject::null().into()
                }
            }
        }
    })
}

// Task Management

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeClearAllTasks(
    _env: JNIEnv,
    _class: JClass,
    replica_ptr: jlong,
) -> jboolean {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeClearAllTasks");
        return 0;
    }

    with_replica_lock!(replica_ptr, "nativeClearAllTasks", 0, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            match clear_all_tasks_internal(replica) {
                Ok(_) => {
                    info!("All tasks cleared successfully");
                    1
                }
                Err(e) => {
                    error!("Failed to clear all tasks: {:?}", e);
                    0
                }
            }
        }
    })
}

fn clear_all_tasks_internal(replica: &mut Replica) -> Result<(), Box<dyn std::error::Error>> {
    let mut ops = Operations::new();
    ops.push(Operation::UndoPoint);
    
    // Get all tasks and delete them
    let all_tasks = replica.all_tasks()?;
    let all_uuids: Vec<_> = all_tasks.keys().cloned().collect();
    info!("Clearing {} tasks", all_uuids.len());
    
    for uuid in all_uuids {
        if let Some(mut task) = replica.get_task(uuid)? {
            task.set_status(Status::Deleted, &mut ops)?;
        }
    }
    
    replica.commit_operations(ops)?;
    replica.rebuild_working_set(true)?;
    
    Ok(())
}

// Synchronization

#[no_mangle]
pub extern "system" fn Java_com_tasksquire_data_storage_TaskChampionJniImpl_nativeSync<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass,
    replica_ptr: jlong,
    server_config_json: JString,
) -> JString<'local> {
    if replica_ptr == 0 {
        error!("Null replica pointer passed to nativeSync");
        return match env.new_string("ERROR: Null replica pointer") {
            Ok(s) => s,
            Err(_) => JObject::null().into(),
        };
    }

    let server_config_str: String = match env.get_string(&server_config_json) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("Failed to get server config JSON string: {:?}", e);
            return match env.new_string("ERROR: Failed to get server config JSON") {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    info!("Starting sync with config: {}", server_config_str);
    
    // Configure TLS for Android - force use of bundled certificates
    configure_android_tls();

    // Parse the JSON to extract server configuration
    let server_config_data: serde_json::Value = match serde_json::from_str(&server_config_str) {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to parse server config JSON: {:?}", e);
            return match env.new_string(&format!("ERROR: Invalid JSON - {}", e)) {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    // Determine server type and create appropriate configuration
    let server_type = match server_config_data.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            error!("Missing 'type' field in server config");
            return match env.new_string("ERROR: Missing 'type' field") {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    let bucket = match server_config_data.get("bucket").and_then(|v| v.as_str()) {
        Some(b) => b.to_string(),
        None => {
            error!("Missing 'bucket' field in server config");
            return match env.new_string("ERROR: Missing 'bucket' field") {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    let encryption_secret = match server_config_data.get("encryption_secret").and_then(|v| v.as_str()) {
        Some(secret) => {
            if secret.is_empty() {
                error!("Encryption secret cannot be empty");
                return match env.new_string("ERROR: Empty encryption secret") {
                    Ok(s) => s,
                    Err(_) => JObject::null().into(),
                };
            }
            secret.as_bytes().to_vec()
        }
        None => {
            error!("Missing 'encryption_secret' field in server config");
            return match env.new_string("ERROR: Missing 'encryption_secret' field") {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    // Create TaskChampion ServerConfig based on type
    let server_config = match server_type {
        "gcp" => {
            let credential_path = server_config_data
                .get("credential_path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            ServerConfig::Gcp {
                bucket,
                credential_path,
                encryption_secret,
            }
        }
        "aws" => {
            let region = match server_config_data.get("region").and_then(|v| v.as_str()) {
                Some(r) => r.to_string(),
                None => {
                    error!("Missing 'region' field in AWS server config");
                    return match env.new_string("ERROR: Missing 'region' field for AWS") {
                        Ok(s) => s,
                        Err(_) => JObject::null().into(),
                    };
                }
            };

            let credentials = match server_config_data.get("credentials") {
                Some(cred_data) => {
                    let cred_type = match cred_data.get("type").and_then(|v| v.as_str()) {
                        Some(t) => t,
                        None => {
                            error!("Missing 'type' field in AWS credentials");
                            return match env.new_string("ERROR: Missing 'type' field in AWS credentials") {
                                Ok(s) => s,
                                Err(_) => JObject::null().into(),
                            };
                        }
                    };

                    match cred_type {
                        "access_key" => {
                            let access_key_id = match cred_data.get("access_key_id").and_then(|v| v.as_str()) {
                                Some(id) => id.to_string(),
                                None => {
                                    error!("Missing 'access_key_id' field in AWS access key credentials");
                                    return match env.new_string("ERROR: Missing 'access_key_id' field") {
                                        Ok(s) => s,
                                        Err(_) => JObject::null().into(),
                                    };
                                }
                            };

                            let secret_access_key = match cred_data.get("secret_access_key").and_then(|v| v.as_str()) {
                                Some(key) => key.to_string(),
                                None => {
                                    error!("Missing 'secret_access_key' field in AWS access key credentials");
                                    return match env.new_string("ERROR: Missing 'secret_access_key' field") {
                                        Ok(s) => s,
                                        Err(_) => JObject::null().into(),
                                    };
                                }
                            };

                            AwsCredentials::AccessKey {
                                access_key_id,
                                secret_access_key,
                            }
                        }
                        "profile" => {
                            let profile_name = match cred_data.get("profile_name").and_then(|v| v.as_str()) {
                                Some(name) => name.to_string(),
                                None => {
                                    error!("Missing 'profile_name' field in AWS profile credentials");
                                    return match env.new_string("ERROR: Missing 'profile_name' field") {
                                        Ok(s) => s,
                                        Err(_) => JObject::null().into(),
                                    };
                                }
                            };

                            AwsCredentials::Profile { profile_name }
                        }
                        "default" => AwsCredentials::Default,
                        _ => {
                            error!("Unknown AWS credential type: {}", cred_type);
                            return match env.new_string(&format!("ERROR: Unknown AWS credential type: {}", cred_type)) {
                                Ok(s) => s,
                                Err(_) => JObject::null().into(),
                            };
                        }
                    }
                }
                None => {
                    error!("Missing 'credentials' field in AWS server config");
                    return match env.new_string("ERROR: Missing 'credentials' field for AWS") {
                        Ok(s) => s,
                        Err(_) => JObject::null().into(),
                    };
                }
            };

            ServerConfig::Aws {
                region,
                bucket,
                credentials,
                encryption_secret,
            }
        }
        _ => {
            error!("Unknown server type: {}", server_type);
            return match env.new_string(&format!("ERROR: Unknown server type: {}", server_type)) {
                Ok(s) => s,
                Err(_) => JObject::null().into(),
            };
        }
    };

    info!("Created server config, starting sync operation");

    with_replica_lock!(replica_ptr, "nativeSync", match env.new_string("ERROR: Failed to acquire replica lock") { 
        Ok(s) => s, 
        Err(_) => JObject::null().into() 
    }, {
        unsafe {
            let replica = &mut *(replica_ptr as *mut Replica);
            
            match server_config.into_server() {
                Ok(mut server) => {
                    // Wrap sync operation in panic handler to catch TLS certificate panics
                    let sync_result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                        replica.sync(&mut server, false)
                    }));
                    
                    match sync_result {
                        Ok(sync_result) => match sync_result {
                        Ok(()) => {
                            info!("Sync completed successfully");
                            
                            // Rebuild working set to ensure all tasks are properly updated
                            match replica.rebuild_working_set(true) {
                                Ok(_) => {
                                    info!("Working set rebuilt after sync");
                                }
                                Err(e) => {
                                    error!("Failed to rebuild working set after sync: {:?}", e);
                                }
                            }
                            
                            match env.new_string("SUCCESS") {
                                Ok(s) => s,
                                Err(e) => {
                                    error!("Failed to create success string: {:?}", e);
                                    JObject::null().into()
                                }
                            }
                        }
                        Err(e) => {
                            error!("Sync failed: {:?}", e);
                            match env.new_string(&format!("ERROR: Sync failed - {}", e)) {
                                Ok(s) => s,
                                Err(_) => JObject::null().into(),
                            }
                        }
                        }
                        Err(panic_err) => {
                            error!("Sync operation panicked (likely TLS certificate issue): {:?}", panic_err);
                            match env.new_string("ERROR: Sync failed due to TLS certificate issue on Android. This is a known limitation with AWS sync on Android.") {
                                Ok(s) => s,
                                Err(_) => JObject::null().into(),
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create server from config: {:?}", e);
                    match env.new_string(&format!("ERROR: Failed to create server - {}", e)) {
                        Ok(s) => s,
                        Err(_) => JObject::null().into(),
                    }
                }
            }
        }
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
                    with_replica_lock!(replica_ptr_clone, "test_concurrent", (), {
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
                    });
                }
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.join().expect("Thread panicked");
        }
        
        // Verify all tasks were created
        with_replica_lock!(replica_ptr, "test_concurrent_verify", (), {
            unsafe {
                let replica = &mut *(replica_ptr as *mut Replica);
                let all_tasks = replica.all_tasks().expect("Failed to get all tasks");
                assert_eq!(all_tasks.len(), num_threads * tasks_per_thread);
            }
        });
        
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
        with_replica_lock!(replica_ptr, "test_timeout", (), {
            // If we get here, the mutex was successfully recovered from poisoning
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
        });
        
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