package com.tasksquire.data.storage;

/**
 * Thrown when a replica cannot be opened or created at the requested
 * data directory (storage initialisation failure).
 */
public class ReplicaInitializationException extends TaskChampionException {
    public ReplicaInitializationException(String message) {
        super(message);
    }
}
