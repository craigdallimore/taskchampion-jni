# TaskChampion JNI

JNI bindings for the [TaskChampion](https://github.com/GothenburgBitFactory/taskchampion) task management library, enabling Android applications to use TaskWarrior-compatible task management.

## Installation

### Option 1: GitHub Releases (Recommended)

Download the latest AAR from [Releases](https://github.com/craigdallimore/taskchampion-jni/releases) and add to your project:

```gradle
dependencies {
    implementation files('libs/taskchampion-jni-0.2.1-alpha.aar')
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
    implementation 'io.github.craigdallimore:taskchampion-jni:0.2.1-alpha'
}
```

## Tests

```
cargo test
```

## License

MIT

This project contains JNI bindings for [TaskChampion](https://github.com/GothenburgBitFactory/taskchampion), which is also licensed under the [MIT License](https://github.com/GothenburgBitFactory/taskchampion?tab=MIT-1-ov-file#readme).
