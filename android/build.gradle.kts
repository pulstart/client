plugins {
    id("com.android.application") version "9.2.1" apply false
}

val rustOutput = layout.projectDirectory.dir("app/build/generated/rustJniLibs")
val home = System.getProperty("user.home")
val cargo = providers.environmentVariable("CARGO").orElse("cargo")
val ndkHome = providers.environmentVariable("ANDROID_NDK_HOME")
    .orElse("$home/Android/Sdk/ndk/29.0.14206865")

tasks.register<Exec>("buildRust") {
    group = "build"
    description = "Build the Android Rust client library"
    workingDir = layout.projectDirectory.dir("rust").asFile
    environment("ANDROID_NDK_HOME", ndkHome.get())
    // audiopus_sys expects ANDROID_NDK and bundles an older libopus CMake project.
    environment("ANDROID_NDK", ndkHome.get())
    environment("CMAKE_POLICY_VERSION_MINIMUM", "3.5")

    inputs.file(layout.projectDirectory.file("rust/Cargo.toml"))
    inputs.file(layout.projectDirectory.file("rust/Cargo.lock"))
    inputs.dir(layout.projectDirectory.dir("rust/src"))
    inputs.file(layout.projectDirectory.file("../core/Cargo.toml"))
    inputs.dir(layout.projectDirectory.dir("../core/src"))
    inputs.file(layout.projectDirectory.file("../protocol/Cargo.toml"))
    inputs.dir(layout.projectDirectory.dir("../protocol/src"))
    outputs.dir(rustOutput)

    doFirst {
        rustOutput.asFile.mkdirs()
    }
    commandLine(
        cargo.get(),
        "ndk",
        "--platform", "24",
        "-t", "armeabi-v7a",
        "-t", "arm64-v8a",
        "-t", "x86_64",
        "-o", rustOutput.asFile.absolutePath,
        "build",
        "--release",
    )
}
