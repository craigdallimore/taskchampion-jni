package com.tasksquire.data.storage;

/**
 * Thrown when a tag string fails TaskChampion's tag-name validation.
 */
public class InvalidTagException extends TaskChampionException {
    public InvalidTagException(String message) {
        super(message);
    }
}
