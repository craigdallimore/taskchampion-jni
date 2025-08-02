package com.tasksquire.data.storage;

/**
 * TaskChampion JNI implementation for Android
 * 
 * This class provides Java bindings for the TaskChampion Rust library,
 * enabling task management functionality in Android applications.
 * 
 * Features:
 * - Task creation, modification, and deletion
 * - Tag and annotation management
 * - Synchronization with cloud storage
 * - Undo/redo operations
 * - Working set management
 */
public class TaskChampionJniImpl {
    
    static {
        System.loadLibrary("taskchampion_jni");
    }
    
    // Lifecycle management
    
    /**
     * Initialize a new TaskChampion replica
     * @param dataDir Directory to store task data
     * @return Pointer to the replica (0 on failure)
     */
    public static native long nativeInitialize(String dataDir);
    
    /**
     * Destroy a TaskChampion replica and free resources
     * @param replicaPtr Pointer to the replica
     */
    public static native void nativeDestroy(long replicaPtr);
    
    // Transaction control
    
    /**
     * Undo the last set of operations
     * @param replicaPtr Pointer to the replica
     * @return true if undo was successful
     */
    public static native boolean nativeUndo(long replicaPtr);
    
    /**
     * Add an undo point for transaction grouping
     * @param replicaPtr Pointer to the replica
     * @param message Description of the undo point
     */
    public static native void nativeAddUndoPoint(long replicaPtr, String message);
    
    /**
     * Commit all pending operations and rebuild working set
     * @param replicaPtr Pointer to the replica
     */
    public static native void nativeCommit(long replicaPtr);
    
    // Task creation and basic operations
    
    /**
     * Create a new task with the given UUID
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID as string
     */
    public static native void nativeCreateTask(long replicaPtr, String uuid);
    
    /**
     * Set the description of a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param description Task description
     */
    public static native void nativeTaskSetDescription(long replicaPtr, String uuid, String description);
    
    /**
     * Set the status of a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param status Task status ("pending", "completed", "deleted")
     */
    public static native void nativeTaskSetStatus(long replicaPtr, String uuid, String status);
    
    // Task property management
    
    /**
     * Set a custom property on a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param key Property key
     * @param value Property value (null to remove)
     */
    public static native void nativeTaskSetValue(long replicaPtr, String uuid, String key, String value);
    
    /**
     * Add a tag to a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param tag Tag to add
     */
    public static native void nativeTaskAddTag(long replicaPtr, String uuid, String tag);
    
    /**
     * Remove a tag from a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param tag Tag to remove
     */
    public static native void nativeTaskRemoveTag(long replicaPtr, String uuid, String tag);
    
    // Annotations
    
    /**
     * Add an annotation to a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param description Annotation description
     */
    public static native void nativeTaskAddAnnotation(long replicaPtr, String uuid, String description);
    
    /**
     * Remove an annotation from a task
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @param entryTimestamp Timestamp of annotation entry
     */
    public static native void nativeTaskRemoveAnnotation(long replicaPtr, String uuid, long entryTimestamp);
    
    // Data retrieval
    
    /**
     * Get all task UUIDs in the replica
     * @param replicaPtr Pointer to the replica
     * @return Array of UUID strings
     */
    public static native String[] nativeGetAllTaskUuids(long replicaPtr);
    
    /**
     * Get task data as JSON string
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @return JSON string containing task data
     */
    public static native String nativeGetTaskData(long replicaPtr, String uuid);
    
    /**
     * Get UUID for a task at the given index in the working set
     * @param replicaPtr Pointer to the replica
     * @param index 1-based index (TaskWarrior style)
     * @return UUID string or null if not found
     */
    public static native String nativeGetUuidForIndex(long replicaPtr, int index);
    
    // Task Management
    
    /**
     * Clear all tasks from the replica by setting them to deleted status.
     * This is useful for switching sync profiles without affecting server data.
     * 
     * @param replicaPtr Pointer to the replica
     * @return true if clearing was successful
     */
    public static native boolean nativeClearAllTasks(long replicaPtr);
    
    // Synchronization
    
    /**
     * Synchronize with remote server
     * @param replicaPtr Pointer to the replica
     * @param serverConfigJson JSON configuration for server
     * @return "SUCCESS" or error message
     */
    public static native String nativeSync(long replicaPtr, String serverConfigJson);
}