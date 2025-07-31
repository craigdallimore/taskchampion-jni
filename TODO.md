- get tasksquire using artifacts from github or whatever
- check if this is published anywhere publically, and tidy that up. Maven? Gradle?
- add outstanding unimplmented APIs
- add documentation for API usage
- add github tags
- make a lovely README


We're implementing per-replica thread safety using a registry-based pattern. Each Replica is wrapped
   in an Arc<Mutex<>> and stored in a global DashMap registry keyed by the JNI pointer. When JNI
  methods are called, we look up the replica in the registry and acquire its individual mutex with a
  5-second timeout to prevent ANRs. This approach provides true concurrency (multiple replicas can
  operate simultaneously) while ensuring each replica's operations are serialized. We minimize lock
  scope by releasing mutexes before JNI array operations and provide robust error handling with
  poisoned mutex recovery. The implementation is transparent to consuming applications—existing code
  works unchanged while gaining full thread safety guarantees.

Thread Safety Implementation Tasks

  Core Implementation

  - Add dependencies: dashmap = "6.0", lazy_static = "1.5"
  - Create ThreadSafeReplica struct with Arc<Mutex<Replica>>
  - Implement global REPLICA_REGISTRY: DashMap<jlong, ThreadSafeReplica>
  - Add lock_replica_with_timeout() helper function (5s timeout)
  - Update nativeInitialize to use registry pattern
  - Update nativeDestroy to remove from registry
  - Update all 11 JNI methods to use timeout locking
  - Add create_empty_string_array() helper function

  Error Handling

  - Return empty arrays instead of null on lock failures
  - Add poisoned mutex recovery
  - Log timeout errors with method names

  Testing

  - Add test_concurrent_task_operations() test
  - Add test_timeout_handling() test
  - Add test_replica_cleanup() test

  Documentation

  - Update README with thread safety guarantees
  - Add usage examples for concurrent access


