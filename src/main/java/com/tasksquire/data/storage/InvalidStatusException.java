package com.tasksquire.data.storage;

/**
 * Thrown when a task status string is not one of the valid values
 * ("pending", "completed", "deleted").
 */
public class InvalidStatusException extends TaskChampionException {
    public InvalidStatusException(String message) {
        super(message);
    }
}
