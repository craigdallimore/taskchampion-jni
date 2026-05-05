package com.tasksquire.data.storage;

/**
 * Thrown when a UUID parameter cannot be parsed as a v4 UUID.
 */
public class InvalidUuidException extends TaskChampionException {
    public InvalidUuidException(String message) {
        super(message);
    }
}
