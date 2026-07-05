package com.tasksquire.data.storage;

/**
 * Java bindings for the <a href="https://github.com/GothenburgBitFactory/taskchampion">TaskChampion</a>
 * task-management library, intended for use from Android (and any other JVM
 * environment) applications.
 *
 * <h2>Capabilities</h2>
 * <ul>
 *   <li>Task creation, modification, and queries</li>
 *   <li>Tag and annotation management</li>
 *   <li>Arbitrary key/value attributes per task</li>
 *   <li>Undo via undo points in the operation journal</li>
 *   <li>Synchronisation with a remote storage server (Google Cloud Storage
 *       or AWS S3-compatible)</li>
 * </ul>
 *
 * <h2>Threading model</h2>
 * <p>All native methods are <strong>synchronous</strong> and may block. They
 * must be called <strong>off the main thread</strong>: a sync operation can
 * involve a network round-trip, and even local operations acquire a
 * per-replica mutex that may be held by another thread.
 *
 * <p>Per-replica operations are <strong>serialised</strong>: a call against
 * a given replica handle is observed atomically with respect to any
 * concurrent call against the same handle from another thread. Operations
 * on <strong>different replica handles proceed concurrently</strong>; one
 * replica's work never blocks another's.
 *
 * <p>Replica handles (the {@code long} returned by {@link #nativeInitialize})
 * may be shared across threads.
 *
 * <h3>Sync and the per-replica lock</h3>
 * <p>The sync methods ({@code nativeSyncGcp}, {@code nativeSyncAwsAccessKey},
 * {@code nativeSyncAwsProfile}, {@code nativeSyncAwsDefault}) hold the
 * per-replica mutex for the full network round-trip — typically a few
 * seconds, longer on flaky connections. While a sync is in progress on a
 * given replica handle, all other operations against that handle queue.
 *
 * <p>For ANR-sensitive consumers, the recommended pattern is to open a
 * <strong>second replica handle</strong> against the same data directory
 * dedicated to sync work. The two handles have independent mutexes, so
 * UI reads against the primary handle proceed regardless of sync activity.
 * The underlying SQLite database uses WAL journalling and serialises
 * concurrent writes safely.
 *
 * <pre>
 * long uiReplica   = nativeInitialize(dataDir);  // user-facing operations
 * long syncReplica = nativeInitialize(dataDir);  // background sync only
 *
 * // On a background thread:
 * nativeSyncGcp(syncReplica, bucket, credPath, secret);
 *
 * // Concurrently, on a different background thread:
 * String[] uuids = nativeGetAllTaskUuids(uiReplica);
 *
 * // After sync completes, optionally refresh the UI replica's
 * // working-set index so newly-merged tasks have indices:
 * nativeRebuildWorkingSet(uiReplica, true);
 * </pre>
 *
 * <p>Both handles must be destroyed via {@link #nativeDestroy} when no
 * longer needed.
 *
 * <h2>Error reporting</h2>
 * <p>Failures are reported as unchecked exceptions in the
 * {@link TaskChampionException} hierarchy. Errors are never silently
 * dropped. The full hierarchy:
 * <ul>
 *   <li>{@link InvalidReplicaException} — null or unregistered handle</li>
 *   <li>{@link InvalidUuidException} — UUID could not be parsed</li>
 *   <li>{@link InvalidStatusException} — status string not in
 *       {@code pending}, {@code completed}, {@code deleted}</li>
 *   <li>{@link InvalidTagException} — tag string failed
 *       TaskChampion's tag-name validation</li>
 *   <li>{@link ReplicaInitializationException} — storage could not be
 *       opened or created</li>
 *   <li>{@link SyncException} — synchronisation failed (invalid config,
 *       transport error, TLS panic, etc.)</li>
 *   <li>{@link TaskChampionStorageException} — anything else from the
 *       underlying library, including missing tasks on a write</li>
 * </ul>
 *
 * <p>A few queries return {@code null} or an empty array to mean "no such
 * value", which is a normal answer rather than a failure:
 * {@link #nativeGetUuidForIndex} returns {@code null} when no task occupies
 * the index, {@link #nativeGetTaskData} returns {@code null} when the task
 * does not exist, {@link #nativeGetAllTaskUuids} and
 * {@link #nativeGetAllTasks} return an empty array when there are no
 * tasks, and {@link #nativeUndo} returns {@code false} when there is
 * nothing to undo. When reading many tasks, prefer the single
 * {@link #nativeGetAllTasks} call over iterating
 * {@link #nativeGetTaskData} per UUID.
 *
 * <p>Rust panics in the underlying library are caught at every native
 * entry point and re-thrown as {@link TaskChampionException}; they will
 * not crash the JVM.
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
     * Add an undo point. The next undo will reverse all operations
     * recorded after this point.
     * @param replicaPtr Pointer to the replica
     */
    public static native void nativeAddUndoPoint(long replicaPtr);
    
    /**
     * Rebuild the working set so that 1-based pending-task indices reflect
     * current task state. Equivalent to TaskChampion's
     * {@code Replica::rebuild_working_set}. Individual write operations
     * commit to the operation journal automatically; this method does
     * <em>not</em> commit pending changes (there are none) but does refresh
     * the index used by {@link #nativeGetUuidForIndex}.
     *
     * @param replicaPtr Pointer to the replica
     * @param renumber if {@code true}, reassign 1-based indices to all
     *                 currently-pending tasks (matching the TaskWarrior
     *                 default after a sync); if {@code false}, retain
     *                 existing indices where possible
     */
    public static native void nativeRebuildWorkingSet(long replicaPtr, boolean renumber);
    
    // Task creation and basic operations
    
    /**
     * Get-or-create a task by UUID, equivalent to TaskChampion's
     * {@code Replica::create_task}. If no task with the given UUID exists,
     * an empty task is created: no fields are set, so status, description,
     * entry, modified etc. are all absent until written (absent status is
     * interpreted as pending by TaskChampion convention). If a task with
     * the UUID already exists, the call is a no-op and the existing task
     * is left unmodified; no exception is thrown.
     *
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
     * Get the full state of every task in the replica in a single call.
     *
     * <p>Each element is one task's JSON document in exactly the format
     * described on {@link #nativeGetTaskData}. Element order is
     * unspecified. Returns an empty array when the replica contains no
     * tasks. Equivalent to TaskChampion's {@code Replica::all_tasks}.
     *
     * <p>This is the preferred way to read many tasks: it acquires the
     * per-replica lock once and crosses the JNI boundary once, rather
     * than once per task as with {@link #nativeGetAllTaskUuids} followed
     * by {@link #nativeGetTaskData} for each UUID.
     *
     * @param replicaPtr Pointer to the replica
     * @return Array of JSON strings, one per task
     */
    public static native String[] nativeGetAllTasks(long replicaPtr);

    /**
     * Get a task's full state as a JSON string.
     *
     * <p>The returned JSON has the following shape:
     * <pre>
     * {
     *   "uuid": "abc-…",
     *   "description": "…",
     *   "status": "pending",
     *   "entry": "1234567890",
     *   "modified": "1234567899",
     *   "tags": ["work", "priority"],
     *   "annotations": [
     *     {"entry": "1234567890", "description": "first note"}
     *   ],
     *   "udas": {
     *     "project": "home",
     *     "priority": "H"
     *   }
     * }
     * </pre>
     *
     * <p>All scalar values are encoded as JSON strings (matching
     * TaskChampion's underlying string-keyed storage). The {@code uuid},
     * {@code tags}, {@code annotations}, and {@code udas} keys are
     * always present; the well-known fields ({@code description},
     * {@code status}, {@code entry}, {@code modified}) appear only when
     * the underlying task has them set. Annotation entries are
     * second-precision Unix timestamps.
     *
     * @param replicaPtr Pointer to the replica
     * @param uuid Task UUID
     * @return JSON string of the task's state, or {@code null} if no
     *         task exists with the given UUID
     */
    public static native String nativeGetTaskData(long replicaPtr, String uuid);
    
    /**
     * Get UUID for a task at the given index in the working set
     * @param replicaPtr Pointer to the replica
     * @param index 1-based index (TaskWarrior style)
     * @return UUID string or null if not found
     */
    public static native String nativeGetUuidForIndex(long replicaPtr, int index);
    
    // Synchronization
    
    /**
     * Synchronise with a Google Cloud Storage bucket.
     *
     * @param replicaPtr Pointer to the replica
     * @param bucket Name of the GCS bucket; must be non-empty
     * @param credentialPath Path to a service-account JSON key file, or
     *                       {@code null} to use ambient credentials
     * @param encryptionSecret Secret used to encrypt the synced payload
     *                         at rest in the bucket; must be non-empty
     * @throws SyncException on any synchronisation failure (including
     *                       invalid configuration and TLS panics)
     * @throws TaskChampionStorageException if the sync exchange
     *                       succeeded but the subsequent working-set
     *                       rebuild failed; the remote payload has
     *                       already been exchanged and the caller may
     *                       retry the rebuild via
     *                       {@link #nativeRebuildWorkingSet}
     * @throws InvalidReplicaException if replicaPtr is null or unregistered
     */
    public static native void nativeSyncGcp(
        long replicaPtr,
        String bucket,
        String credentialPath,
        String encryptionSecret
    );

    /**
     * Synchronise with an AWS S3-compatible bucket using an explicit
     * access-key credential pair.
     *
     * @param replicaPtr Pointer to the replica
     * @param region AWS region (e.g. "us-east-1")
     * @param bucket Name of the S3 bucket; must be non-empty
     * @param accessKeyId AWS access key ID
     * @param secretAccessKey AWS secret access key
     * @param encryptionSecret Secret used to encrypt the synced payload;
     *                         must be non-empty
     * @throws SyncException on any synchronisation failure
     * @throws TaskChampionStorageException if the sync exchange
     *                       succeeded but the subsequent working-set
     *                       rebuild failed; the remote payload has
     *                       already been exchanged and the caller may
     *                       retry the rebuild via
     *                       {@link #nativeRebuildWorkingSet}
     * @throws InvalidReplicaException if replicaPtr is null or unregistered
     */
    public static native void nativeSyncAwsAccessKey(
        long replicaPtr,
        String region,
        String bucket,
        String accessKeyId,
        String secretAccessKey,
        String encryptionSecret
    );

    /**
     * Synchronise with an AWS S3-compatible bucket using a named
     * credential profile from the host's AWS config.
     *
     * @param replicaPtr Pointer to the replica
     * @param region AWS region
     * @param bucket Name of the S3 bucket; must be non-empty
     * @param profileName Name of the AWS profile to use
     * @param encryptionSecret Secret used to encrypt the synced payload;
     *                         must be non-empty
     * @throws SyncException on any synchronisation failure
     * @throws TaskChampionStorageException if the sync exchange
     *                       succeeded but the subsequent working-set
     *                       rebuild failed; the remote payload has
     *                       already been exchanged and the caller may
     *                       retry the rebuild via
     *                       {@link #nativeRebuildWorkingSet}
     * @throws InvalidReplicaException if replicaPtr is null or unregistered
     */
    public static native void nativeSyncAwsProfile(
        long replicaPtr,
        String region,
        String bucket,
        String profileName,
        String encryptionSecret
    );

    /**
     * Synchronise with an AWS S3-compatible bucket using the default
     * AWS credential chain (environment variables, shared credentials
     * file, EC2 instance metadata, etc.).
     *
     * @param replicaPtr Pointer to the replica
     * @param region AWS region
     * @param bucket Name of the S3 bucket; must be non-empty
     * @param encryptionSecret Secret used to encrypt the synced payload;
     *                         must be non-empty
     * @throws SyncException on any synchronisation failure
     * @throws TaskChampionStorageException if the sync exchange
     *                       succeeded but the subsequent working-set
     *                       rebuild failed; the remote payload has
     *                       already been exchanged and the caller may
     *                       retry the rebuild via
     *                       {@link #nativeRebuildWorkingSet}
     * @throws InvalidReplicaException if replicaPtr is null or unregistered
     */
    public static native void nativeSyncAwsDefault(
        long replicaPtr,
        String region,
        String bucket,
        String encryptionSecret
    );
}