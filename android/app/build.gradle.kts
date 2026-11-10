plugins {
    id("com.android.application")
}

val stApiUrl = providers.gradleProperty("stApiUrl")
    .orElse("https://st-api.kubemaxx.io")
    .get()
val escapedStApiUrl = stApiUrl.replace("\\", "\\\\").replace("\"", "\\\"")
// One signing key for every build — local or CI, debug or release — so any APK
// installs over any other. It is committed and its password is public on
// purpose: it says "a build of this repository", not who published it. Each
// CI runner used to sign with its own throwaway debug key, and no release
// installed over the previous one. `ST_ANDROID_KEYSTORE`,
// `ST_ANDROID_STORE_PASSWORD`, `ST_ANDROID_KEY_ALIAS` and
// `ST_ANDROID_KEY_PASSWORD` (all four) swap in a private key.
val privateKey = listOf(
    "ST_ANDROID_KEYSTORE",
    "ST_ANDROID_STORE_PASSWORD",
    "ST_ANDROID_KEY_ALIAS",
    "ST_ANDROID_KEY_PASSWORD",
).map { providers.environmentVariable(it).orNull }
val (signingStorePath, signingStorePassword, signingKeyAlias, signingKeyPassword) =
    if (privateKey.none { it.isNullOrBlank() }) {
        privateKey.map { it!! }
    } else {
        listOf("st-release.p12", "st-android", "st", "st-android")
    }

// Android refuses an APK with a lower versionCode than the installed one, so
// a local build takes the newest release tag's. CI stamps the two version
// lines below from the tag it builds.
val releaseTag = runCatching {
    providers.exec {
        commandLine("git", "describe", "--tags", "--match", "v[0-9]*", "--abbrev=0")
        isIgnoreExitValue = true
    }.standardOutput.asText.get().trim().removePrefix("v")
}.getOrDefault("")
val releaseVersion = releaseTag.substringBefore('-').split('.').mapNotNull(String::toIntOrNull)
val stVersionCode = if (releaseVersion.size == 3) {
    releaseVersion[0] * 1_000_000 + releaseVersion[1] * 1_000 + releaseVersion[2]
} else {
    1
}
val stVersionName = if (releaseVersion.size == 3) releaseTag else "0.0.0-dev"

android {
    namespace = "io.kubemaxx.st"
    compileSdk = 36
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.kubemaxx.st"
        minSdk = 24
        targetSdk = 36
        versionCode = stVersionCode
        versionName = stVersionName
        buildConfigField("String", "ST_API_URL", "\"$escapedStApiUrl\"")

        ndk {
            abiFilters += listOf("armeabi-v7a", "arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        create("st") {
            storeFile = file(signingStorePath)
            storePassword = signingStorePassword
            keyAlias = signingKeyAlias
            keyPassword = signingKeyPassword
        }
    }

    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.getByName("st")
        }
        getByName("debug") {
            signingConfig = signingConfigs.getByName("st")
        }
    }

    sourceSets {
        getByName("main").jniLibs.directories.add(
            layout.buildDirectory.dir("generated/rustJniLibs").get().asFile.absolutePath
        )
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        buildConfig = true
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

dependencies {
    implementation("androidx.activity:activity:1.13.0")
    testImplementation("junit:junit:4.13.2")
    // The android.jar used for local unit tests stubs org.json, so every call
    // throws "not mocked". Pull in the real implementation so JSON parsing can
    // be tested without an instrumented device.
    testImplementation("org.json:json:20250517")
}

tasks.named("preBuild") {
    dependsOn(rootProject.tasks.named("buildRust"))
}
