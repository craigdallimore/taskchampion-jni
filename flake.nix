{
  description = "TaskChampion JNI Bindings Development Environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    android-nixpkgs = {
      url = "github:tadfisher/android-nixpkgs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, android-nixpkgs, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ rust-overlay.overlays.default ];
        };

        rust-toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [
            "aarch64-linux-android"
            "armv7-linux-androideabi"
            "i686-linux-android"
            "x86_64-linux-android"
          ];
        };

        android-sdk = android-nixpkgs.sdk.${system} (sdkPkgs: with sdkPkgs; [
          cmdline-tools-latest
          build-tools-35-0-0
          build-tools-34-0-0
          platform-tools
          platforms-android-34
          platforms-android-33
          ndk-25-2-9519653
        ]);
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            # Android development
            android-sdk
            jdk17
            gradle

            # Rust toolchain with Android targets
            rust-toolchain

            # Build tools
            cmake
            gnumake
            pkg-config

            # Development tools
            git
            curl
            wget
            unzip
            file
            binutils
            nixpkgs-fmt
          ];

          shellHook = ''
            export ANDROID_HOME="${android-sdk}/share/android-sdk"
            export ANDROID_SDK_ROOT="$ANDROID_HOME"
            export ANDROID_NDK_ROOT="${android-sdk}/share/android-sdk/ndk/25.2.9519653"
            export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"
            export JAVA_HOME="${pkgs.jdk17}/lib/openjdk"

            # Rust + Android cross-compilation configuration
            export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${android-sdk}/share/android-sdk/ndk/25.2.9519653/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang++"
            export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="${android-sdk}/share/android-sdk/ndk/25.2.9519653/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi21-clang++"
            export CARGO_TARGET_I686_LINUX_ANDROID_LINKER="${android-sdk}/share/android-sdk/ndk/25.2.9519653/toolchains/llvm/prebuilt/linux-x86_64/bin/i686-linux-android21-clang++"
            export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="${android-sdk}/share/android-sdk/ndk/25.2.9519653/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android21-clang++"

            # Override AAPT2 to use the nix-shipped version (fixes NixOS dynamic linking issue)
            export GRADLE_OPTS="-Dorg.gradle.project.android.aapt2FromMavenOverride=$ANDROID_HOME/build-tools/35.0.0/aapt2"

            # Set up gradle properties
            mkdir -p ~/.gradle
            cat > ~/.gradle/gradle.properties << EOF
# Android SDK location
sdk.dir=$ANDROID_HOME

# Gradle daemon
org.gradle.daemon=true
org.gradle.parallel=true
org.gradle.configureondemand=true

# Gradle JVM
org.gradle.jvmargs=-Xmx4g -XX:+UseG1GC -XX:+UseStringDeduplication
EOF

            echo "🦀 TaskChampion JNI Development Environment Ready!"
            echo "📱 Android SDK: $ANDROID_HOME"
            echo "🔧 Android NDK: $ANDROID_NDK_ROOT"
            echo "☕ Java: $JAVA_HOME"
            echo "🦀 Rust: $(rustc --version 2>/dev/null)"
          '';
        };
      });
}
