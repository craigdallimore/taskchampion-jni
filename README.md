# TaskChampion JNI

JNI bindings for the [TaskChampion](https://github.com/GothenburgBitFactory/taskchampion) task management library, enabling Android applications to use TaskWarrior-compatible task management.

## Installation

### Option 1: GitHub Releases (Recommended)

Download the latest AAR from [Releases](https://github.com/craigdallimore/taskchampion-jni/releases) and add to your project:

```gradle
dependencies {
    implementation files('libs/taskchampion-jni-<version>.aar')
}
```

### Option 2: GitHub Packages (Requires Authentication)

```gradle
repositories {
    maven {
        url = uri("https://maven.pkg.github.com/craigdallimore/taskchampion-jni")
        credentials {
            username = project.findProperty("gpr.user") ?: System.getenv("GPR_USER")
            password = project.findProperty("gpr.key") ?: System.getenv("GPR_TOKEN")
        }
    }
}

dependencies {
    implementation 'io.github.craigdallimore:taskchampion-jni:<version>'
}
```

## Concurrency

Every native call is synchronous and blocks its calling thread. Calls against
the same replica handle are serialised, and a sync holds that serialisation
for its entire duration — the network round-trip plus the post-sync
working-set rebuild — so a single-handle application will see every other
call stall while a sync is in flight.

To keep a UI responsive: make all calls off the main thread, and open two
replica handles over the same data directory — one for user-facing
operations, one for background sync. Operations on distinct handles proceed
independently. See the `TaskChampionJniImpl` Javadoc and
`specs/taskchampion-jni.allium` for the full concurrency contract.

## Tests

```
cargo test
```

## License

MIT

This project contains JNI bindings for [TaskChampion](https://github.com/GothenburgBitFactory/taskchampion), which is also licensed under the [MIT License](https://github.com/GothenburgBitFactory/taskchampion?tab=MIT-1-ov-file#readme).
