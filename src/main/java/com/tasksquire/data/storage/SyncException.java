package com.tasksquire.data.storage;

/**
 * Thrown when synchronisation with a remote storage server fails for
 * any reason (invalid configuration, transport error, TLS panic, etc.).
 */
public class SyncException extends TaskChampionException {
    public SyncException(String message) {
        super(message);
    }
}
