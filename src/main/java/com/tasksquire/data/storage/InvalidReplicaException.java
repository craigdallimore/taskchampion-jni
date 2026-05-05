package com.tasksquire.data.storage;

/**
 * Thrown when a method is called with a replica handle that is null
 * or no longer registered (e.g., already destroyed, or never returned
 * by nativeInitialize).
 */
public class InvalidReplicaException extends TaskChampionException {
    public InvalidReplicaException(String message) {
        super(message);
    }
}
