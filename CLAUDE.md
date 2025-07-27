# Context

This project is some JNI bindings for the taskchampion library.
It was extracted from the tasksquire project where it was integrated with an android app.
For local development this repo has a nix flake to establish the environment.
The github repo for this project is at https://github.com/craigdallimore/taskchampion-jni
You can use $GITHUB_TOKEN to do things like fetching workflow run logs from github via curl (note logs are zipped).

## Objectives for this project

It is intended to be published such that android developers can use it to integrate taskchampion into their apps.
At present the full taskchampion feature set has not been wrapped, this is something we are working on.
When the github repo is updated a new artifact is expected to be built.
The tasksquire project needs to be updated to use these artifacts; when tasksquire is missing a feature provided by this project we will add it.
