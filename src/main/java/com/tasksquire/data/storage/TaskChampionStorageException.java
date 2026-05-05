package com.tasksquire.data.storage;

/**
 * Thrown when the underlying TaskChampion library reports an error
 * not captured by a more specific exception type (storage I/O,
 * operation commit failure, missing task on a write, etc.).
 */
public class TaskChampionStorageException extends TaskChampionException {
    public TaskChampionStorageException(String message) {
        super(message);
    }
}
