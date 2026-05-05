package com.tasksquire.data.storage;

/**
 * Base unchecked exception type for all errors raised by the
 * TaskChampion JNI binding. All other binding-specific exceptions
 * extend this class.
 */
public class TaskChampionException extends RuntimeException {
    public TaskChampionException(String message) {
        super(message);
    }
}
