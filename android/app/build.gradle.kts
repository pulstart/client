plugins {
    id("com.android.application")
}

val stApiUrl = providers.gradleProperty("stApiUrl")
    .orElse("https://st-api.kubemaxx.io")
    .get()
val escapedStApiUrl = stApiUrl.replace("\\", "\\\\").replace("\"", "\\\"")
val releaseStorePath = providers.environmentVariable("ST_ANDROID_KEYSTORE").orNull
val releaseStorePassword = providers.environmentVariable("ST_ANDROID_STORE_PASSWORD").orNull
val releaseKeyAlias = providers.environmentVariable("ST_ANDROID_KEY_ALIAS").orNull
val releaseKeyPassword = providers.environmentVariable("ST_ANDROID_KEY_PASSWORD").orNull
val hasReleaseKey = listOf(
    releaseStorePath,
    releaseStorePassword,
    releaseKeyAlias,
    releaseKeyPassword,
).all { !it.isNullOrBlank() }

android {
    namespace = "io.kubemaxx.st"
    compileSdk = 36
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.kubemaxx.st"
        minSdk = 24
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        buildConfigField("String", "ST_API_URL", "\"$escapedStApiUrl\"")

        ndk {
            abiFilters += listOf("armeabi-v7a", "arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        if (hasReleaseKey) {
            create("localRelease") {
                storeFile = file(releaseStorePath!!)
                storePassword = releaseStorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        getByName("release") {
            // Local/manual distribution must still be signed. Use an explicitly
            // configured stable key when present, otherwise Android's local debug key.
            signingConfig = signingConfigs.getByName(if (hasReleaseKey) "localRelease" else "debug")
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
}

tasks.named("preBuild") {
    dependsOn(rootProject.tasks.named("buildRust"))
}
